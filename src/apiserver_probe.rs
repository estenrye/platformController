use crate::secret_rewrite::{KubeSecretStore, SecretKey, SecretStore};
use k8s_openapi::api::core::v1::{Node, Secret};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams};
use kube::{Api, Client, Config};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

pub const APISERVER_PORT: u16 = 6443;
/// The apiserver's serving certificate very probably does not list node IPs but
/// does list this name. Unverified on Talos; a failure is "cannot verify".
pub const TLS_SERVER_NAME: &str = "kubernetes.default.svc";
pub const CANARY_NAME: &str = "etcd-encryption-canary";
pub const CANARY_NAMESPACE: &str = "kube-system";

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Reading every Secret (one GET each) is the slow call; give it much longer.
const LIST_TIMEOUT: Duration = Duration::from_secs(900);
/// The label every EndpointSlice of the `default/kubernetes` Service carries.
const KUBERNETES_SERVICE_SLICES: &str = "kubernetes.io/service-name=kubernetes";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeTarget {
    pub name: String,
    pub address: String,
}

#[derive(thiserror::Error, Debug)]
pub enum ProbeError {
    #[error("{0}")]
    Request(String),
    #[error("probe timed out after {0:?}")]
    Timeout(Duration),
}

/// Everything the verification needs from a cluster, per apiserver. A trait so
/// the orchestration is unit-testable with an in-memory fake.
#[allow(async_fn_in_trait)]
pub trait ApiserverProbe {
    async fn control_plane_nodes(&self) -> Result<Vec<NodeTarget>, ProbeError>;
    /// Every address in the `default/kubernetes` EndpointSlices: the
    /// apiservers that registered themselves, labelled or not.
    async fn apiserver_endpoint_addresses(&self) -> Result<Vec<String>, ProbeError>;
    /// `kms-providers` is present and ok in `/readyz?verbose` on this node.
    async fn readyz_kms(&self, node: &NodeTarget) -> Result<bool, ProbeError>;
    /// The node's raw `/metrics` body.
    async fn metrics(&self, node: &NodeTarget) -> Result<String, ProbeError>;
    /// Write the canary Secret through this node's apiserver.
    async fn canary_write(&self, node: &NodeTarget) -> Result<(), ProbeError>;
    /// Read every Secret from etcd through this node: page through the names,
    /// then GET each Secret with no resourceVersion (served from storage, not
    /// the watch cache, so the apiserver decrypts each one under its stored
    /// prefix). A Secret deleted since it was listed (404) is skipped and not
    /// counted; any other error is an `Err`. Returns how many were read.
    async fn list_all_secrets(&self, node: &NodeTarget) -> Result<u64, ProbeError>;
    /// Write and read back the canary through the normal API endpoint.
    async fn canary_round_trip(&self) -> Result<bool, ProbeError>;
}

/// Whether `/readyz?verbose` has a passing `kms-providers` line.
pub fn kms_check_ok(readyz_body: &str) -> bool {
    readyz_body.lines().any(|line| line.trim() == "[+]kms-providers ok")
}

pub fn apiserver_url(address: &str) -> String {
    if address.contains(':') {
        format!("https://[{address}]:{APISERVER_PORT}")
    } else {
        format!("https://{address}:{APISERVER_PORT}")
    }
}

/// The control-plane Nodes' InternalIPs, in the order given. Nodes without the
/// control-plane label are skipped. A control-plane node without an InternalIP
/// is kept with an EMPTY address (verification reports it unverifiable): it is
/// still an apiserver, and dropping it would let the rest look like the whole.
pub fn node_targets(nodes: &[Node]) -> Vec<NodeTarget> {
    nodes
        .iter()
        .filter(|n| {
            n.metadata
                .labels
                .as_ref()
                .is_some_and(|l| l.contains_key("node-role.kubernetes.io/control-plane"))
        })
        .filter_map(|n| {
            let address = n
                .status
                .as_ref()
                .and_then(|s| s.addresses.as_ref())
                .and_then(|a| a.iter().find(|a| a.type_ == "InternalIP"))
                .map(|a| a.address.clone())
                .unwrap_or_default();
            Some(NodeTarget { name: n.metadata.name.clone()?, address })
        })
        .collect()
}

/// Every address of every endpoint in these EndpointSlices, in order.
pub fn endpoint_addresses(slices: &[EndpointSlice]) -> Vec<String> {
    slices.iter().flat_map(|s| s.endpoints.iter().flatten()).flat_map(|e| e.addresses.iter().cloned()).collect()
}

/// The same IP address however it is spelled (`fd00::11` and
/// `fd00:0:0:0:0:0:0:11`); otherwise exact string equality.
pub fn same_address(a: &str, b: &str) -> bool {
    match (a.parse::<std::net::IpAddr>(), b.parse::<std::net::IpAddr>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The outcome of one uncached Secret GET.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretRead {
    /// Read (so decrypted) from etcd; the object itself is dropped at once.
    Read,
    /// Deleted since it was listed (404): nothing to read, not counted.
    Gone,
}

/// Pages through `store`'s Secret names and calls `get` for each one. Returns
/// how many were read; a `Gone` is skipped and not counted; any list or get
/// error is an `Err` naming the Secret (never its data).
pub async fn read_every_secret<S, G, F>(store: &S, mut get: G) -> Result<u64, ProbeError>
where
    S: SecretStore,
    G: FnMut(SecretKey) -> F,
    F: std::future::Future<Output = Result<SecretRead, ProbeError>>,
{
    let mut read = 0u64;
    let mut token: Option<String> = None;
    loop {
        let page = store.list_page(token.as_deref()).await.map_err(request_err)?;
        for key in page.keys {
            let id = format!("{}/{}", key.namespace, key.name);
            match get(key).await {
                Ok(SecretRead::Read) => read += 1,
                Ok(SecretRead::Gone) => {}
                Err(err) => return Err(ProbeError::Request(format!("get secret {id}: {err}"))),
            }
        }
        match page.next {
            Some(next) => token = Some(next),
            None => return Ok(read),
        }
    }
}

pub fn canary_secret(value: &str) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(CANARY_NAME.to_string()),
            namespace: Some(CANARY_NAMESPACE.to_string()),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([("probe".to_string(), value.to_string())])),
        ..Default::default()
    }
}

async fn bounded_for<T>(limit: Duration, fut: impl std::future::Future<Output = T>) -> Result<T, ProbeError> {
    timeout_at(Instant::now() + limit, fut).await.map_err(|_| ProbeError::Timeout(limit))
}

async fn bounded<T>(fut: impl std::future::Future<Output = T>) -> Result<T, ProbeError> {
    bounded_for(PROBE_TIMEOUT, fut).await
}

fn request_err(err: impl std::fmt::Display) -> ProbeError {
    ProbeError::Request(err.to_string())
}

/// Talks to each control-plane apiserver directly, with the controller's own
/// service account, so counters and reads are per-apiserver, not whichever one
/// the load balancer picks.
pub struct KubeApiserverProbe {
    client: Client,
    base: Config,
}

impl KubeApiserverProbe {
    pub fn new(client: Client) -> Result<Self, ProbeError> {
        let base = Config::incluster().map_err(request_err)?;
        Ok(KubeApiserverProbe { client, base })
    }

    fn node_client(&self, node: &NodeTarget) -> Result<Client, ProbeError> {
        let mut config = self.base.clone();
        config.cluster_url = apiserver_url(&node.address).parse().map_err(request_err)?;
        config.tls_server_name = Some(TLS_SERVER_NAME.to_string());
        Client::try_from(config).map_err(request_err)
    }

    async fn get_text(&self, node: &NodeTarget, path: &str) -> Result<String, ProbeError> {
        let client = self.node_client(node)?;
        let request = http::Request::get(path).body(Vec::new()).map_err(request_err)?;
        bounded(client.request_text(request)).await?.map_err(request_err)
    }
}

impl ApiserverProbe for KubeApiserverProbe {
    async fn control_plane_nodes(&self) -> Result<Vec<NodeTarget>, ProbeError> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        let list = bounded(nodes.list(&ListParams::default().labels("node-role.kubernetes.io/control-plane")))
            .await?
            .map_err(request_err)?;
        Ok(node_targets(&list.items))
    }

    async fn apiserver_endpoint_addresses(&self) -> Result<Vec<String>, ProbeError> {
        let slices: Api<EndpointSlice> = Api::namespaced(self.client.clone(), "default");
        let list = bounded(slices.list(&ListParams::default().labels(KUBERNETES_SERVICE_SLICES)))
            .await?
            .map_err(request_err)?;
        Ok(endpoint_addresses(&list.items))
    }

    async fn readyz_kms(&self, node: &NodeTarget) -> Result<bool, ProbeError> {
        Ok(kms_check_ok(&self.get_text(node, "/readyz?verbose").await?))
    }

    async fn metrics(&self, node: &NodeTarget) -> Result<String, ProbeError> {
        self.get_text(node, "/metrics").await
    }

    async fn canary_write(&self, node: &NodeTarget) -> Result<(), ProbeError> {
        let api: Api<Secret> = Api::namespaced(self.node_client(node)?, CANARY_NAMESPACE);
        let secret = canary_secret(&chrono::Utc::now().to_rfc3339());
        let params = PatchParams::apply("platform-controller").force();
        let patch = Patch::Apply(&secret);
        bounded(api.patch(CANARY_NAME, &params, &patch)).await?.map_err(request_err)?;
        Ok(())
    }

    async fn list_all_secrets(&self, node: &NodeTarget) -> Result<u64, ProbeError> {
        // A limit-paged LIST may be served from the watch cache (Kubernetes
        // >= 1.33), which decrypts nothing, so it only supplies the names. A
        // GET with no resourceVersion is served from etcd, decrypting the
        // Secret under its stored prefix. The Secret is dropped unread.
        let client = self.node_client(node)?;
        let store = KubeSecretStore::new(client.clone());
        let get = |key: SecretKey| {
            let api: Api<Secret> = Api::namespaced(client.clone(), &key.namespace);
            async move {
                match bounded(api.get(&key.name)).await? {
                    Ok(_secret) => Ok(SecretRead::Read),
                    Err(kube::Error::Api(status)) if status.code == 404 => Ok(SecretRead::Gone),
                    Err(err) => Err(request_err(err)),
                }
            }
        };
        bounded_for(LIST_TIMEOUT, read_every_secret(&store, get)).await?
    }

    async fn canary_round_trip(&self) -> Result<bool, ProbeError> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), CANARY_NAMESPACE);
        let value = chrono::Utc::now().to_rfc3339();
        let secret = canary_secret(&value);
        let params = PatchParams::apply("platform-controller").force();
        let patch = Patch::Apply(&secret);
        bounded(api.patch(CANARY_NAME, &params, &patch)).await?.map_err(request_err)?;
        let read = bounded(api.get(CANARY_NAME)).await?.map_err(request_err)?;
        let stored = read.data.and_then(|data| data.get("probe").map(|bytes| bytes.0.clone()));
        Ok(stored.as_deref() == Some(value.as_bytes()))
    }
}

/// Best effort: the canary is a probe, not state.
pub async fn delete_canary(client: &Client) {
    let api: Api<Secret> = Api::namespaced(client.clone(), CANARY_NAMESPACE);
    match bounded(api.delete(CANARY_NAME, &DeleteParams::default())).await {
        Ok(Ok(_)) => {}
        Ok(Err(kube::Error::Api(status))) if status.code == 404 => {}
        Ok(Err(err)) => tracing::warn!(error = %err, "failed to delete the canary Secret"),
        Err(err) => tracing::warn!(error = %err, "failed to delete the canary Secret"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kms_check_ok_is_true_only_for_a_passing_kms_providers_line() {
        let ok = "[+]ping ok\n[+]kms-providers ok\n[+]shutdown ok\nreadyz check passed\n";

        assert!(kms_check_ok(ok));
    }

    #[test]
    fn kms_check_ok_is_false_when_the_check_is_absent_or_failing() {
        assert!(!kms_check_ok("[+]ping ok\n[+]shutdown ok\nreadyz check passed\n"));
        assert!(!kms_check_ok("[+]ping ok\n[-]kms-providers failed: reason withheld\n"));
        assert!(!kms_check_ok(""));
    }

    #[test]
    fn apiserver_url_brackets_ipv6_addresses() {
        assert_eq!(apiserver_url("10.0.0.11"), "https://10.0.0.11:6443");
        assert_eq!(apiserver_url("fd00::11"), "https://[fd00::11]:6443");
    }

    fn node(name: &str, labels: &[(&str, &str)], addresses: &[(&str, &str)]) -> Node {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {
                "name": name,
                "labels": labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<std::collections::BTreeMap<_, _>>(),
            },
            "status": {
                "addresses": addresses.iter().map(|(t, a)| serde_json::json!({"type": t, "address": a})).collect::<Vec<_>>(),
            }
        }))
        .unwrap()
    }

    #[test]
    fn node_targets_are_the_control_plane_nodes_internal_ips() {
        let nodes = [
            node("cp1", &[("node-role.kubernetes.io/control-plane", "")], &[("Hostname", "cp1"), ("InternalIP", "10.0.0.11")]),
            node("worker", &[], &[("InternalIP", "10.0.0.21")]),
            node("cp2", &[("node-role.kubernetes.io/control-plane", "")], &[("InternalIP", "10.0.0.12"), ("ExternalIP", "1.2.3.4")]),
        ];

        assert_eq!(
            node_targets(&nodes),
            vec![
                NodeTarget { name: "cp1".to_string(), address: "10.0.0.11".to_string() },
                NodeTarget { name: "cp2".to_string(), address: "10.0.0.12".to_string() },
            ]
        );
    }

    #[test]
    fn a_control_plane_node_without_an_internal_ip_is_kept_with_an_empty_address() {
        // Final review I1: skipping it would silently drop an apiserver.
        let nodes = [node("cp1", &[("node-role.kubernetes.io/control-plane", "")], &[("Hostname", "cp1")])];

        assert_eq!(node_targets(&nodes), vec![NodeTarget { name: "cp1".to_string(), address: String::new() }]);
    }

    #[test]
    fn a_control_plane_node_without_any_status_is_kept_with_an_empty_address() {
        let n: Node = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": { "name": "cp1", "labels": { "node-role.kubernetes.io/control-plane": "" } }
        }))
        .unwrap();

        assert_eq!(node_targets(&[n]), vec![NodeTarget { name: "cp1".to_string(), address: String::new() }]);
    }

    fn slice(addresses: &[&[&str]]) -> EndpointSlice {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": { "name": "kubernetes", "namespace": "default" },
            "addressType": "IPv4",
            "endpoints": addresses.iter().map(|a| serde_json::json!({ "addresses": a })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn endpoint_addresses_are_every_address_of_every_endpoint_in_every_slice() {
        let slices = [slice(&[&["10.0.0.11"], &["10.0.0.12", "10.0.0.13"]]), slice(&[&["fd00::11"]])];

        assert_eq!(endpoint_addresses(&slices), ["10.0.0.11", "10.0.0.12", "10.0.0.13", "fd00::11"]);
    }

    #[test]
    fn same_address_compares_ips_and_falls_back_to_strings() {
        assert!(same_address("fd00::11", "fd00:0:0:0:0:0:0:11"));
        assert!(same_address("10.0.0.1", "10.0.0.1"));
        assert!(!same_address("10.0.0.1", "10.0.0.2"));
        assert!(same_address("cp1.example", "cp1.example"));
        assert!(!same_address("", "10.0.0.1"));
    }

    use crate::secret_rewrite::{SecretKey, SecretPage, SecretStore, StoreError, TouchOutcome};
    use std::sync::Mutex;

    /// Two pages of names; any page fetch after the scripted ones panics.
    struct Pages(Mutex<Vec<Result<SecretPage, StoreError>>>);

    impl SecretStore for Pages {
        async fn list_page(&self, _token: Option<&str>) -> Result<SecretPage, StoreError> {
            self.0.lock().unwrap().remove(0)
        }
        async fn touch(&self, _key: &SecretKey) -> Result<TouchOutcome, StoreError> {
            panic!("the reader never writes")
        }
    }

    fn key(ns: &str, name: &str) -> SecretKey {
        SecretKey { namespace: ns.to_string(), name: name.to_string() }
    }

    fn two_pages() -> Pages {
        Pages(Mutex::new(vec![
            Ok(SecretPage { keys: vec![key("a", "s1"), key("a", "s2")], next: Some("t".to_string()) }),
            Ok(SecretPage { keys: vec![key("b", "s3")], next: None }),
        ]))
    }

    #[tokio::test]
    async fn read_every_secret_gets_each_listed_secret_and_counts_the_reads() {
        let seen = Mutex::new(Vec::new());

        let n = read_every_secret(&two_pages(), |k: SecretKey| {
            seen.lock().unwrap().push(format!("{}/{}", k.namespace, k.name));
            async { Ok(SecretRead::Read) }
        })
        .await
        .unwrap();

        assert_eq!(n, 3);
        assert_eq!(*seen.lock().unwrap(), ["a/s1", "a/s2", "b/s3"]);
    }

    #[tokio::test]
    async fn read_every_secret_skips_secrets_deleted_since_listed_without_counting_them() {
        let n = read_every_secret(&two_pages(), |k: SecretKey| async move {
            Ok(if k.name == "s2" { SecretRead::Gone } else { SecretRead::Read })
        })
        .await
        .unwrap();

        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn read_every_secret_fails_on_any_other_get_error() {
        // A 500 from a Secret no provider can decrypt must never read as clean.
        let err = read_every_secret(&two_pages(), |k: SecretKey| async move {
            if k.name == "s3" {
                Err(ProbeError::Request("500: failed to decrypt".to_string()))
            } else {
                Ok(SecretRead::Read)
            }
        })
        .await
        .unwrap_err();

        assert!(err.to_string().contains("b/s3") && err.to_string().contains("decrypt"), "{err}");
    }

    #[tokio::test]
    async fn read_every_secret_fails_on_a_list_error() {
        let pages = Pages(Mutex::new(vec![
            Ok(SecretPage { keys: vec![key("a", "s1")], next: Some("t".to_string()) }),
            Err(StoreError("410 Gone: continue token expired".to_string())),
        ]));

        assert!(read_every_secret(&pages, |_k: SecretKey| async { Ok(SecretRead::Read) }).await.is_err());
    }

    #[test]
    fn canary_is_a_kube_system_secret_carrying_the_probe_value() {
        let secret = canary_secret("v1");

        assert_eq!(secret.metadata.name.as_deref(), Some("etcd-encryption-canary"));
        assert_eq!(secret.metadata.namespace.as_deref(), Some("kube-system"));
        assert_eq!(secret.string_data.as_ref().unwrap().get("probe").map(String::as_str), Some("v1"));
    }
}
