use crate::etcd_encryption::{EtcdEncryptionSpec, EtcdEncryptionSpecError, KmsProviderKind};
use kube::api::DynamicObject;

/// Host directory the plugin's unix socket lives in; mounted into the plugin
/// pod and (via the Talos patch) into the kube-apiserver static pod.
pub const KMS_SOCKET_DIR: &str = "/var/lib/kms";

/// Namespace the plugin runs in. Talos exempts `kube-system` from Pod Security.
pub const PLUGIN_NAMESPACE: &str = "kube-system";

/// Everything provider-specific the engine-agnostic core needs. Adding a
/// provider means one builder returning this plus one `KmsProviderKind` arm.
#[derive(Debug, Clone)]
pub struct KmsPlan {
    /// The `name` in the `EncryptionConfiguration` KMS provider block.
    pub provider_name: &'static str,
    /// The unix socket the apiserver must reach, as a host path.
    pub socket_path: String,
    pub daemonset_name: String,
    pub daemonset: DynamicObject,
}

impl KmsPlan {
    /// The KMS entry of the `EncryptionConfiguration`'s `providers` list.
    pub fn provider_block(&self) -> serde_json::Value {
        serde_json::json!({
            "kms": {
                "apiVersion": "v2",
                "name": self.provider_name,
                "endpoint": format!("unix://{}", self.socket_path),
                "timeout": "3s",
            }
        })
    }
}

pub fn kms_plan(spec: &EtcdEncryptionSpec) -> Result<KmsPlan, EtcdEncryptionSpecError> {
    match spec.provider {
        KmsProviderKind::Barbican => {
            let barbican = spec.barbican.as_ref().ok_or(EtcdEncryptionSpecError::MissingProviderBlock)?;
            Ok(crate::kms_barbican::plan(barbican))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::{BarbicanSpec, EtcdEncryptionSpec, KmsProviderKind, SecretNameRef};
    use crate::crd::PlatformKind;

    fn spec() -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind: PlatformKind::TalosLinux,
            provider: KmsProviderKind::Barbican,
            barbican: Some(BarbicanSpec {
                image: "img:1".to_string(),
                cloud_config_secret_ref: SecretNameRef { name: "cc".to_string() },
            }),
            acknowledgements: Default::default(),
        }
    }

    #[test]
    fn provider_block_is_a_kms_v2_block_pointing_at_the_unix_socket() {
        let plan = kms_plan(&spec()).unwrap();

        assert_eq!(
            plan.provider_block(),
            serde_json::json!({
                "kms": {
                    "apiVersion": "v2",
                    "name": "barbican",
                    "endpoint": "unix:///var/lib/kms/kms.sock",
                    "timeout": "3s"
                }
            })
        );
    }

    #[test]
    fn kms_plan_without_the_provider_block_is_an_error_not_a_panic() {
        let mut spec = spec();
        spec.barbican = None;

        assert!(kms_plan(&spec).is_err());
    }
}
