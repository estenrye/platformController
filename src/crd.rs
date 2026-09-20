use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CniInstallation",
    status = "CniInstallationStatus",
    shortname = "cni"
)]
#[serde(rename_all = "camelCase")]
pub struct CniInstallationSpec {
    pub platform_kind: PlatformKind,
    pub provider: CniProvider,
    pub calico: CalicoSpec,
    /// How long to wait, during cleanup, for the CNI provider's own managed
    /// resources (e.g. Calico's `calico-node` DaemonSet) to actually disappear
    /// before this controller removes the provider's operator itself. Not
    /// nested under `calico`: any future provider's cleanup would need the
    /// same kind of bounded wait.
    #[serde(default = "default_cleanup_timeout_seconds")]
    pub cleanup_timeout_seconds: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformKind {
    TalosLinux,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CniProvider {
    Calico,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CalicoSpec {
    pub chart_version: String,
    #[serde(default)]
    pub bgp_enabled: bool,
    #[serde(default)]
    pub api_server_enabled: bool,
    #[serde(default)]
    pub ip_pools: Vec<CalicoIpPoolSpec>,
    #[serde(default)]
    pub node_address_autodetection_v6_cidrs: Vec<String>,
    /// BGP configuration and peers. Requires `bgpEnabled: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgp: Option<BgpSpec>,
    /// LoadBalancer-only IP pools (native Calico LoadBalancer IPAM, Calico >= 3.30).
    #[serde(default)]
    pub load_balancer_pools: Vec<LoadBalancerPoolSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CalicoIpPoolSpec {
    pub name: String,
    pub cidr: String,
    #[serde(default)]
    pub encapsulation: Encapsulation,
    #[serde(default = "default_nat_outgoing")]
    pub nat_outgoing: bool,
    /// Omitted means "use Calico's default for this pool's address family",
    /// resolved at render time (26 for IPv4, 122 for IPv6) because a single
    /// fixed serde default cannot be right for both families.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_size: Option<i32>,
    #[serde(default = "default_node_selector")]
    pub node_selector: String,
}

impl CalicoIpPoolSpec {
    /// The block size to render: the explicit value, else the default for the
    /// CIDR's family. An unparseable CIDR falls back to the IPv4 default;
    /// `spec_validation` rejects such a spec before anything renders.
    pub fn effective_block_size(&self) -> i32 {
        self.block_size.unwrap_or_else(|| {
            crate::cidr::parse(&self.cidr)
                .map(|cidr| cidr.family())
                .unwrap_or(crate::cidr::Family::V4)
                .default_block_size()
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BgpSpec {
    pub as_number: u32,
    #[serde(default = "default_true")]
    pub node_to_node_mesh_enabled: bool,
    #[serde(default)]
    pub log_severity_screen: LogSeverity,
    /// CIDRs of LoadBalancer VIP pools to advertise to BGP peers.
    #[serde(default, rename = "serviceLoadBalancerIPs")]
    pub service_load_balancer_ips: Vec<String>,
    #[serde(default)]
    pub peers: Vec<BgpPeerSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BgpPeerSpec {
    pub name: String,
    #[serde(rename = "peerIP")]
    pub peer_ip: String,
    pub as_number: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancerPoolSpec {
    pub name: String,
    pub cidr: String,
    #[serde(default = "default_node_selector")]
    pub node_selector: String,
    /// Retired pools stay declared with `disabled: true`: Calico's pool CIDR is
    /// immutable, so renumbering is a new-pool swap, not an in-place edit.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum LogSeverity {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
    Fatal,
}

fn default_true() -> bool {
    true
}

fn default_nat_outgoing() -> bool {
    true
}

fn default_node_selector() -> String {
    "all()".to_string()
}

fn default_cleanup_timeout_seconds() -> u32 {
    60
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum Encapsulation {
    #[serde(rename = "IPIP")]
    Ipip,
    #[serde(rename = "VXLAN")]
    Vxlan,
    #[default]
    #[serde(rename = "None")]
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CniInstallationStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub chart_version: String,
    #[serde(default)]
    pub applied_resources: Vec<AppliedResourceRef>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Pending,
    Installing,
    Ready,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct AppliedResourceRef {
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub namespace: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn spec_round_trips_through_json() {
        let json = serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "calico",
            "calico": {
                "chartVersion": "v3.29.1",
                "bgpEnabled": true,
                "apiServerEnabled": true,
                "ipPools": [{
                    "name": "pods-v6",
                    "cidr": "fd97:45c2:b3a1:1100::/56",
                    "encapsulation": "None",
                    "natOutgoing": true,
                    "blockSize": 122,
                    "nodeSelector": "all()"
                }],
                "nodeAddressAutodetectionV6Cidrs": ["fd97:45c2:b3a1:179::/64"]
            }
        });

        let spec: CniInstallationSpec = serde_json::from_value(json).expect("spec should deserialize");

        assert_eq!(spec.platform_kind, PlatformKind::TalosLinux);
        assert_eq!(spec.provider, CniProvider::Calico);
        assert!(spec.calico.bgp_enabled);
        assert!(spec.calico.api_server_enabled);
        assert_eq!(spec.calico.ip_pools.len(), 1);
        assert_eq!(spec.calico.ip_pools[0].block_size, Some(122));
        assert_eq!(spec.calico.ip_pools[0].encapsulation, Encapsulation::None);
        assert_eq!(
            spec.calico.node_address_autodetection_v6_cidrs,
            vec!["fd97:45c2:b3a1:179::/64".to_string()]
        );
    }

    #[test]
    fn ip_pool_defaults_apply_when_omitted() {
        let json = serde_json::json!({
            "name": "default",
            "cidr": "10.244.0.0/16"
        });

        let pool: CalicoIpPoolSpec = serde_json::from_value(json).expect("pool should deserialize");

        assert_eq!(pool.encapsulation, Encapsulation::None);
        assert!(pool.nat_outgoing);
        assert_eq!(pool.block_size, None);
        assert_eq!(pool.effective_block_size(), 26);
        assert_eq!(pool.node_selector, "all()");
    }

    #[test]
    fn effective_block_size_defaults_by_address_family() {
        let v6: CalicoIpPoolSpec = serde_json::from_value(serde_json::json!({
            "name": "pods-v6",
            "cidr": "fd00:db8:0:1100::/56"
        }))
        .expect("pool should deserialize");
        let explicit: CalicoIpPoolSpec = serde_json::from_value(serde_json::json!({
            "name": "pods-v6",
            "cidr": "fd00:db8:0:1100::/56",
            "blockSize": 124
        }))
        .expect("pool should deserialize");

        assert_eq!(v6.effective_block_size(), 122);
        assert_eq!(explicit.effective_block_size(), 124);
    }

    #[test]
    fn bgp_spec_defaults_apply_when_omitted() {
        let bgp: BgpSpec = serde_json::from_value(serde_json::json!({ "asNumber": 64514 }))
            .expect("bgp should deserialize");

        assert_eq!(bgp.as_number, 64514);
        assert!(bgp.node_to_node_mesh_enabled);
        assert_eq!(bgp.log_severity_screen, LogSeverity::Info);
        assert!(bgp.service_load_balancer_ips.is_empty());
        assert!(bgp.peers.is_empty());
    }

    #[test]
    fn load_balancer_pool_defaults_apply_when_omitted() {
        let pool: LoadBalancerPoolSpec = serde_json::from_value(serde_json::json!({
            "name": "lb-internal-routed",
            "cidr": "fd00:db8:0:f00::/112"
        }))
        .expect("pool should deserialize");

        assert_eq!(pool.node_selector, "all()");
        assert!(!pool.disabled);
    }

    #[test]
    fn calico_spec_without_bgp_or_load_balancer_pools_still_deserializes() {
        let spec: CalicoSpec = serde_json::from_value(serde_json::json!({ "chartVersion": "v3.29.1" }))
            .expect("existing specs must keep working");

        assert!(spec.bgp.is_none());
        assert!(spec.load_balancer_pools.is_empty());
    }

    #[test]
    fn full_ipv6_spec_with_bgp_and_load_balancer_pools_deserializes() {
        let spec: CalicoSpec = serde_json::from_value(serde_json::json!({
            "chartVersion": "v3.32.1",
            "bgpEnabled": true,
            "bgp": {
                "asNumber": 64514,
                "logSeverityScreen": "Warning",
                "serviceLoadBalancerIPs": ["fd00:db8:0:f00::/112"],
                "peers": [{ "name": "gateway", "peerIP": "fd00:db8:0:179::1", "asNumber": 64512 }]
            },
            "loadBalancerPools": [
                { "name": "lb-internal-routed", "cidr": "fd00:db8:0:f00::/112", "disabled": true }
            ]
        }))
        .expect("spec should deserialize");

        let bgp = spec.bgp.expect("bgp should be present");
        assert_eq!(bgp.log_severity_screen, LogSeverity::Warning);
        assert_eq!(bgp.peers[0].peer_ip, "fd00:db8:0:179::1");
        assert_eq!(bgp.peers[0].as_number, 64512);
        assert_eq!(spec.load_balancer_pools[0].name, "lb-internal-routed");
        assert!(spec.load_balancer_pools[0].disabled);
    }

    #[test]
    fn cleanup_timeout_seconds_defaults_when_omitted() {
        let json = serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "calico",
            "calico": {
                "chartVersion": "v3.29.1"
            }
        });

        let spec: CniInstallationSpec = serde_json::from_value(json).expect("spec should deserialize");

        assert_eq!(spec.cleanup_timeout_seconds, 60);
    }

    #[test]
    fn cleanup_timeout_seconds_can_be_overridden() {
        let json = serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "calico",
            "calico": {
                "chartVersion": "v3.29.1"
            },
            "cleanupTimeoutSeconds": 5
        });

        let spec: CniInstallationSpec = serde_json::from_value(json).expect("spec should deserialize");

        assert_eq!(spec.cleanup_timeout_seconds, 5);
    }

    #[test]
    fn crd_definition_has_expected_group_and_kind() {
        let crd = CniInstallation::crd();
        assert_eq!(crd.spec.group, "platform.rye.ninja");
        assert_eq!(crd.spec.names.kind, "CniInstallation");
        assert_eq!(crd.spec.scope, "Cluster");
    }

    #[test]
    fn condition_serializes_with_correct_camel_case() {
        let condition = Condition {
            type_: "Ready".to_string(),
            status: "True".to_string(),
            reason: "Installed".to_string(),
            message: "Calico installed successfully".to_string(),
            last_transition_time: "2026-09-18T12:00:00Z".to_string(),
            observed_generation: Some(1),
        };

        let json = serde_json::to_value(&condition).expect("should serialize");

        // Verify the JSON has camelCase keys as expected
        assert!(json.get("type").is_some(), "JSON should have 'type' key");
        assert!(json.get("status").is_some(), "JSON should have 'status' key");
        assert!(json.get("reason").is_some(), "JSON should have 'reason' key");
        assert!(json.get("message").is_some(), "JSON should have 'message' key");
        assert!(json.get("lastTransitionTime").is_some(), "JSON should have 'lastTransitionTime' key");
        assert!(json.get("observedGeneration").is_some(), "JSON should have 'observedGeneration' key");

        // Verify values are preserved
        assert_eq!(json.get("type").unwrap().as_str(), Some("Ready"));
        assert_eq!(json.get("status").unwrap().as_str(), Some("True"));
    }

    #[test]
    fn crd_schema_includes_conditions_in_status() {
        let crd = CniInstallation::crd();

        // Navigate to the schema for the status subresource
        let schema = crd.spec.versions[0]
            .schema
            .as_ref()
            .expect("schema should exist");

        let openapi_schema = schema
            .open_api_v3_schema
            .as_ref()
            .expect("openapi_v3_schema should exist");

        // Convert to serde_json::Value to easily navigate the JSON structure
        let schema_json = serde_json::to_value(openapi_schema)
            .expect("should convert schema to JSON");

        // Navigate to status.properties.conditions
        let status_properties = schema_json
            .get("properties")
            .and_then(|p| p.get("status"))
            .and_then(|s| s.get("properties"))
            .expect("status.properties should exist");

        assert!(
            status_properties.get("conditions").is_some(),
            "status.properties should include 'conditions'"
        );
    }
}
