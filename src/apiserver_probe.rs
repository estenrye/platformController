use crate::secret_rewrite::{verify_listable, KubeSecretStore};
use k8s_openapi::api::core::v1::{Node, Secret};
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
/// A full list of every Secret is the slow call; give it longer.
const LIST_TIMEOUT: Duration = Duration::from_secs(300);

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
    /// `kms-providers` is present and ok in `/readyz?verbose` on this node.
    async fn readyz_kms(&self, node: &NodeTarget) -> Result<bool, ProbeError>;
    /// The node's raw `/metrics` body.
    async fn metrics(&self, node: &NodeTarget) -> Result<String, ProbeError>;
    /// Write the canary Secret through this node's apiserver.
    async fn canary_write(&self, node: &NodeTarget) -> Result<(), ProbeError>;
    /// List every Secret through this node (limit-paged, so read from etcd);
    /// returns how many were listed.
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
/// control-plane label or without an InternalIP are skipped.
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
                .as_ref()?
                .addresses
                .as_ref()?
                .iter()
                .find(|a| a.type_ == "InternalIP")?
                .address
                .clone();
            Some(NodeTarget { name: n.metadata.name.clone()?, address })
        })
        .collect()
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
        let store = KubeSecretStore::new(self.node_client(node)?);
        bounded_for(LIST_TIMEOUT, verify_listable(&store)).await?.map_err(request_err)
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
    fn a_control_plane_node_without_an_internal_ip_is_skipped() {
        let nodes = [node("cp1", &[("node-role.kubernetes.io/control-plane", "")], &[("Hostname", "cp1")])];

        assert!(node_targets(&nodes).is_empty());
    }

    #[test]
    fn canary_is_a_kube_system_secret_carrying_the_probe_value() {
        let secret = canary_secret("v1");

        assert_eq!(secret.metadata.name.as_deref(), Some("etcd-encryption-canary"));
        assert_eq!(secret.metadata.namespace.as_deref(), Some("kube-system"));
        assert_eq!(secret.string_data.as_ref().unwrap().get("probe").map(String::as_str), Some("v1"));
    }
}
