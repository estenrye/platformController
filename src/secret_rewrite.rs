use k8s_openapi::api::core::v1::Secret;
use kube::api::{ListParams, PostParams};
use kube::{Api, Client};

/// A Secret's identity. Deliberately carries no data: the rewrite must never
/// hold, log or report a Secret's contents beyond the moment of the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretKey {
    pub namespace: String,
    pub name: String,
}

pub struct SecretPage {
    pub keys: Vec<SecretKey>,
    pub next: Option<String>,
}

pub enum TouchOutcome {
    Rewritten,
    /// Deleted since it was listed: nothing left to encrypt.
    Gone,
    /// Changed since it was read (HTTP 409): safe to retry.
    Conflict,
}

#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct StoreError(pub String);

/// The cluster's Secrets, as the rewrite loop sees them. A trait so the loop
/// is unit-testable without a cluster; `KubeSecretStore` is the real one.
#[allow(async_fn_in_trait)]
pub trait SecretStore {
    async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError>;
    /// Re-save the Secret unchanged so the apiserver stores it through the
    /// current encryption provider.
    async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError>;
}

/// Attempts per Secret before a persistent 409 is counted as a failure.
pub const CONFLICT_RETRIES: usize = 3;

pub struct PageOutcome {
    pub seen: u64,
    pub rewritten: u64,
    pub failed: Vec<SecretKey>,
    pub next: Option<String>,
}

/// Rewrites one page. A listing error aborts (the whole step is retried);
/// a per-Secret error is recorded and the page continues. Idempotent, so a
/// controller restart simply starts over.
pub async fn rewrite_page<S: SecretStore>(
    store: &S,
    token: Option<&str>,
) -> Result<PageOutcome, StoreError> {
    let page = store.list_page(token).await?;
    let mut outcome = PageOutcome {
        seen: page.keys.len() as u64,
        rewritten: 0,
        failed: Vec::new(),
        next: page.next,
    };
    for key in page.keys {
        let mut attempts = 0;
        loop {
            match store.touch(&key).await {
                Ok(TouchOutcome::Rewritten) | Ok(TouchOutcome::Gone) => {
                    outcome.rewritten += 1;
                    break;
                }
                Ok(TouchOutcome::Conflict) => {
                    attempts += 1;
                    if attempts >= CONFLICT_RETRIES {
                        outcome.failed.push(key.clone());
                        break;
                    }
                }
                Err(_) => {
                    outcome.failed.push(key.clone());
                    break;
                }
            }
        }
    }
    Ok(outcome)
}

const PAGE_SIZE: u32 = 100;

pub struct KubeSecretStore {
    api: Api<Secret>,
}

impl KubeSecretStore {
    pub fn new(client: Client) -> Self {
        KubeSecretStore { api: Api::all(client) }
    }
}

impl SecretStore for KubeSecretStore {
    async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError> {
        let mut params = ListParams::default().limit(PAGE_SIZE);
        if let Some(token) = token {
            params = params.continue_token(token);
        }
        let list = self.api.list(&params).await.map_err(|err| StoreError(err.to_string()))?;
        let keys = list
            .items
            .iter()
            .map(|secret| SecretKey {
                namespace: secret.metadata.namespace.clone().unwrap_or_default(),
                name: secret.metadata.name.clone().unwrap_or_default(),
            })
            .collect();
        let next = list.metadata.continue_.filter(|token| !token.is_empty());
        Ok(SecretPage { keys, next })
    }

    async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError> {
        // Namespaced api for the write; `self.api` is cluster-wide.
        let namespaced: Api<Secret> = Api::namespaced(self.api.clone().into_client(), &key.namespace);
        let secret = match namespaced.get(&key.name).await {
            Ok(secret) => secret,
            Err(kube::Error::Api(status)) if status.code == 404 => return Ok(TouchOutcome::Gone),
            Err(err) => return Err(StoreError(err.to_string())),
        };
        // `replace` carries the object's resourceVersion, so a concurrent
        // change is a 409 rather than a lost update. The apiserver rewrites a
        // stored object whose on-disk form is stale for the current provider,
        // which is exactly what this is for.
        match namespaced.replace(&key.name, &PostParams::default(), &secret).await {
            Ok(_) => Ok(TouchOutcome::Rewritten),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(TouchOutcome::Gone),
            Err(kube::Error::Api(status)) if status.code == 409 => Ok(TouchOutcome::Conflict),
            Err(err) => Err(StoreError(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    fn key(name: &str) -> SecretKey {
        SecretKey { namespace: "ns".to_string(), name: name.to_string() }
    }

    #[derive(Default)]
    struct FakeStore {
        keys: Vec<SecretKey>,
        page_size: usize,
        /// name -> remaining conflicts to return before succeeding
        conflicts: Mutex<HashMap<String, usize>>,
        gone: HashSet<String>,
        denied: HashSet<String>,
        touched: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn with(names: &[&str], page_size: usize) -> Self {
            FakeStore { keys: names.iter().map(|n| key(n)).collect(), page_size, ..Default::default() }
        }
    }

    impl SecretStore for FakeStore {
        async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError> {
            let start: usize = token.map(|t| t.parse().unwrap()).unwrap_or(0);
            let end = (start + self.page_size).min(self.keys.len());
            Ok(SecretPage {
                keys: self.keys[start..end].to_vec(),
                next: (end < self.keys.len()).then(|| end.to_string()),
            })
        }

        async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError> {
            if self.denied.contains(&key.name) {
                return Err(StoreError("admission webhook denied the request".to_string()));
            }
            if self.gone.contains(&key.name) {
                return Ok(TouchOutcome::Gone);
            }
            let mut conflicts = self.conflicts.lock().unwrap();
            if let Some(remaining) = conflicts.get_mut(&key.name)
                && *remaining > 0
            {
                *remaining -= 1;
                return Ok(TouchOutcome::Conflict);
            }
            self.touched.lock().unwrap().push(key.name.clone());
            Ok(TouchOutcome::Rewritten)
        }
    }

    #[tokio::test]
    async fn rewrites_every_secret_on_a_page_and_reports_the_next_token() {
        let store = FakeStore::with(&["a", "b", "c"], 2);

        let first = rewrite_page(&store, None).await.unwrap();

        assert_eq!((first.seen, first.rewritten), (2, 2));
        assert!(first.failed.is_empty());
        assert_eq!(first.next.as_deref(), Some("2"));

        let second = rewrite_page(&store, first.next.as_deref()).await.unwrap();

        assert_eq!((second.seen, second.rewritten), (1, 1));
        assert_eq!(second.next, None);
        assert_eq!(*store.touched.lock().unwrap(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn a_conflict_is_retried_and_then_succeeds() {
        // Review Focus 1.
        let store = FakeStore::with(&["a"], 10);
        store.conflicts.lock().unwrap().insert("a".to_string(), 2);

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.rewritten, 1);
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn a_conflict_that_never_clears_is_counted_failed_after_bounded_retries() {
        let store = FakeStore::with(&["a"], 10);
        store.conflicts.lock().unwrap().insert("a".to_string(), 1000);

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.rewritten, 0);
        assert_eq!(outcome.failed, vec![key("a")]);
    }

    #[tokio::test]
    async fn a_secret_deleted_mid_run_is_done_not_failed() {
        // Review Focus 1.
        let mut store = FakeStore::with(&["a", "b"], 10);
        store.gone.insert("a".to_string());

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.rewritten, 2);
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn a_webhook_denial_fails_that_secret_but_the_rest_are_still_rewritten() {
        // Review Focus 2.
        let mut store = FakeStore::with(&["a", "b", "c"], 10);
        store.denied.insert("b".to_string());

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.seen, 3);
        assert_eq!(outcome.rewritten, 2);
        assert_eq!(outcome.failed, vec![key("b")]);
        assert_eq!(*store.touched.lock().unwrap(), ["a", "c"]);
    }

    #[tokio::test]
    async fn an_empty_cluster_is_a_complete_empty_page() {
        let store = FakeStore::with(&[], 10);

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!((outcome.seen, outcome.rewritten, outcome.next), (0, 0, None));
    }
}
