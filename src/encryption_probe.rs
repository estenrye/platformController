use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, ObjectMeta, Patch, PatchParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

/// Metric name prefixes the apiserver only emits samples for once a KMS
/// provider is configured and in use. Open verification item 1 in the spec:
/// confirm the exact names against a real cluster and tighten this list.
pub const KMS_METRIC_PREFIXES: &[&str] = &["apiserver_envelope_encryption_"];

pub const CANARY_NAME: &str = "etcd-encryption-canary";
pub const CANARY_NAMESPACE: &str = "kube-system";

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(thiserror::Error, Debug)]
pub enum ProbeError {
    #[error("{0}")]
    Request(String),
    #[error("probe timed out after {0:?}")]
    Timeout(Duration),
}

/// Whether the apiserver's `/metrics` body has a sample (not a `# HELP` /
/// `# TYPE` comment) from a KMS metric family.
pub fn metrics_report_kms(metrics: &str) -> bool {
    metrics
        .lines()
        .filter(|line| !line.starts_with('#'))
        .any(|line| KMS_METRIC_PREFIXES.iter().any(|prefix| line.starts_with(prefix)))
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

async fn bounded<T>(fut: impl std::future::Future<Output = T>) -> Result<T, ProbeError> {
    timeout_at(Instant::now() + PROBE_TIMEOUT, fut)
        .await
        .map_err(|_| ProbeError::Timeout(PROBE_TIMEOUT))
}

/// Reads the apiserver's own `/metrics` through the Kubernetes API.
pub async fn kms_active(client: &Client) -> Result<bool, ProbeError> {
    let request = http::Request::get("/metrics")
        .body(Vec::new())
        .map_err(|err| ProbeError::Request(err.to_string()))?;
    let body = bounded(client.request_text(request))
        .await?
        .map_err(|err| ProbeError::Request(err.to_string()))?;
    Ok(metrics_report_kms(&body))
}

/// Writes a canary Secret and reads it back. Conclusive that the apiserver can
/// round-trip under its current encryption configuration; it cannot prove what
/// is on disk in etcd (that is not readable through the Kubernetes API).
pub async fn canary_round_trips(client: &Client) -> Result<bool, ProbeError> {
    let api: Api<Secret> = Api::namespaced(client.clone(), CANARY_NAMESPACE);
    let value = chrono::Utc::now().to_rfc3339();
    let secret = canary_secret(&value);
    let params = PatchParams::apply("platform-controller").force();
    let patch = Patch::Apply(&secret);
    let write = api.patch(CANARY_NAME, &params, &patch);
    bounded(write).await?.map_err(|err| ProbeError::Request(err.to_string()))?;
    let read = bounded(api.get(CANARY_NAME))
        .await?
        .map_err(|err| ProbeError::Request(err.to_string()))?;
    let stored = read.data.and_then(|data| data.get("probe").map(|bytes| bytes.0.clone()));
    Ok(stored.as_deref() == Some(value.as_bytes()))
}

/// Best effort: the canary is a probe, not state, so a failure here is only logged.
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
    fn metrics_with_an_envelope_encryption_sample_report_kms() {
        let metrics = "# HELP x y\napiserver_request_total{code=\"200\"} 5\napiserver_envelope_encryption_key_id_hash_total{provider_name=\"barbican\"} 3\n";

        assert!(metrics_report_kms(metrics));
    }

    #[test]
    fn metrics_without_one_do_not_report_kms() {
        let metrics = "# HELP apiserver_envelope_encryption_dek_cache_fill_percent a comment line\napiserver_request_total 5\n";

        assert!(!metrics_report_kms(metrics));
    }

    #[test]
    fn an_empty_body_does_not_report_kms() {
        assert!(!metrics_report_kms(""));
    }

    #[test]
    fn canary_is_a_kube_system_secret_carrying_the_probe_value() {
        let secret = canary_secret("v1");

        assert_eq!(secret.metadata.name.as_deref(), Some("etcd-encryption-canary"));
        assert_eq!(secret.metadata.namespace.as_deref(), Some("kube-system"));
        assert_eq!(
            secret.string_data.as_ref().unwrap().get("probe").map(String::as_str),
            Some("v1")
        );
    }
}
