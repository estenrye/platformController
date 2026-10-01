use crate::kms_provider::{KmsPlan, KMS_SOCKET_DIR};
use serde_json::{json, Value};

// The shape of these documents follows Talos's `KubeEtcdEncryptionConfig`
// reference (`config` holds the EncryptionConfiguration minus apiVersion/kind)
// and `cluster.apiServer.extraVolumes`. Neither the KMS provider block nor the
// socket mount has been exercised against a live Talos node yet: both are
// open verification items in the spec, settled by
// docs/runbooks/etcd-encryption-verification.md.

fn identity() -> Value {
    json!({ "identity": {} })
}

fn render(providers: Vec<Value>, with_socket_volume: bool) -> String {
    let encryption = json!({
        "apiVersion": "v1alpha1",
        "kind": "KubeEtcdEncryptionConfig",
        "config": {
            "resources": [{ "resources": ["secrets"], "providers": providers }],
        },
    });
    let encryption = serde_yaml::to_string(&encryption).expect("patch serializes to YAML");
    if !with_socket_volume {
        return encryption;
    }
    let volume = json!({
        "cluster": { "apiServer": { "extraVolumes": [
            { "hostPath": KMS_SOCKET_DIR, "mountPath": KMS_SOCKET_DIR },
        ] } },
    });
    let volume = serde_yaml::to_string(&volume).expect("patch serializes to YAML");
    format!("{volume}---\n{encryption}")
}

/// Patch 1: KMS first (new writes are encrypted), identity second (existing
/// plaintext stays readable).
pub fn enable_kms(plan: &KmsPlan) -> String {
    render(vec![plan.provider_block(), identity()], true)
}

/// Patch 2: KMS only -- plaintext is no longer accepted.
pub fn remove_identity(plan: &KmsPlan) -> String {
    render(vec![plan.provider_block()], true)
}

/// Deletion step 1: identity first (new writes are plaintext) but KMS kept so
/// existing ciphertext stays readable until every Secret is rewritten.
pub fn revert(plan: &KmsPlan) -> String {
    render(vec![identity(), plan.provider_block()], true)
}

/// Deletion step 2, only after every Secret was rewritten plaintext.
pub fn remove_kms() -> String {
    render(vec![identity()], false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::BarbicanSpec;
    use crate::etcd_encryption::SecretNameRef;

    fn plan() -> KmsPlan {
        crate::kms_barbican::plan(&BarbicanSpec {
            image: "img:1".to_string(),
            cloud_config_secret_ref: SecretNameRef { name: "cc".to_string() },
        })
    }

    /// The `providers` list of the KubeEtcdEncryptionConfig document, as the
    /// first key of each entry ("kms" / "identity").
    fn provider_order(patch: &str) -> Vec<String> {
        let doc = patch
            .split("---\n")
            .map(|d| serde_yaml::from_str::<serde_json::Value>(d).unwrap())
            .find(|d| d["kind"] == "KubeEtcdEncryptionConfig")
            .expect("has a KubeEtcdEncryptionConfig document");
        doc["config"]["resources"][0]["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_object().unwrap().keys().next().unwrap().clone())
            .collect()
    }

    #[test]
    fn enable_kms_lists_kms_first_so_identity_can_still_read_old_plaintext() {
        assert_eq!(provider_order(&enable_kms(&plan())), ["kms", "identity"]);
    }

    #[test]
    fn remove_identity_leaves_only_kms() {
        assert_eq!(provider_order(&remove_identity(&plan())), ["kms"]);
    }

    #[test]
    fn revert_lists_identity_first_but_keeps_kms_so_old_ciphertext_stays_readable() {
        // Review Focus 5: identity-only here would make every KMS-encrypted
        // Secret unreadable.
        assert_eq!(provider_order(&revert(&plan())), ["identity", "kms"]);
    }

    #[test]
    fn remove_kms_is_identity_only() {
        assert_eq!(provider_order(&remove_kms()), ["identity"]);
    }

    #[test]
    fn the_kms_block_points_at_the_plugin_socket_and_covers_secrets() {
        let patch = enable_kms(&plan());
        let doc = patch
            .split("---\n")
            .map(|d| serde_yaml::from_str::<serde_json::Value>(d).unwrap())
            .find(|d| d["kind"] == "KubeEtcdEncryptionConfig")
            .unwrap();

        assert_eq!(doc["apiVersion"], "v1alpha1");
        assert_eq!(doc["config"]["resources"][0]["resources"], serde_json::json!(["secrets"]));
        assert_eq!(
            doc["config"]["resources"][0]["providers"][0]["kms"]["endpoint"],
            "unix:///var/lib/kms/kms.sock"
        );
    }

    #[test]
    fn kms_bearing_patches_mount_the_socket_dir_into_the_apiserver() {
        for patch in [enable_kms(&plan()), remove_identity(&plan()), revert(&plan())] {
            let volumes = patch
                .split("---\n")
                .map(|d| serde_yaml::from_str::<serde_json::Value>(d).unwrap())
                .find_map(|d| d["cluster"]["apiServer"]["extraVolumes"].as_array().cloned())
                .expect("has an extraVolumes document");

            assert_eq!(volumes[0]["hostPath"], "/var/lib/kms");
            assert_eq!(volumes[0]["mountPath"], "/var/lib/kms");
        }
    }

    #[test]
    fn remove_kms_has_no_volume_document() {
        assert!(!remove_kms().contains("extraVolumes"));
    }
}
