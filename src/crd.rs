use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
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

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
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
    #[serde(default = "default_block_size")]
    pub block_size: i32,
    #[serde(default = "default_node_selector")]
    pub node_selector: String,
}

fn default_nat_outgoing() -> bool {
    true
}

fn default_block_size() -> i32 {
    112
}

fn default_node_selector() -> String {
    "all()".to_string()
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
    #[schemars(skip)]
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
        assert_eq!(spec.calico.ip_pools[0].block_size, 122);
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
        assert_eq!(pool.block_size, 112);
        assert_eq!(pool.node_selector, "all()");
    }

    #[test]
    fn crd_definition_has_expected_group_and_kind() {
        let crd = CniInstallation::crd();
        assert_eq!(crd.spec.group, "platform.rye.ninja");
        assert_eq!(crd.spec.names.kind, "CniInstallation");
        assert_eq!(crd.spec.scope, "Cluster");
    }
}
