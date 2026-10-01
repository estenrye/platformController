use crate::crd::{Condition, PlatformKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "EtcdEncryption",
    status = "EtcdEncryptionStatus",
    shortname = "etcdenc"
)]
#[serde(rename_all = "camelCase")]
pub struct EtcdEncryptionSpec {
    pub platform_kind: PlatformKind,
    /// The `name` of the kms entry in your apiserver EncryptionConfiguration.
    /// The target storage prefix is `k8s:enc:kms:v2:<name>:`. Defaults to `""`
    /// so a CR left over from v0.1.11 still deserializes; validation rejects it.
    #[serde(default)]
    pub kms_provider_name: String,
    /// `Disabled` (default) only observes. `Enabled` lets the controller
    /// rewrite every Secret once the apiserver writes with the target provider.
    #[serde(default)]
    pub rewrite: RewriteMode,
    #[serde(default)]
    pub acknowledgements: Acknowledgements,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum RewriteMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Acknowledgements {
    /// Set true after removing the legacy providers (e.g. secretbox) from the
    /// apiserver's EncryptionConfiguration. The metrics cannot prove this; it is
    /// the operator's statement.
    #[serde(default)]
    pub legacy_providers_removed: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum EncryptionPhase {
    #[default]
    Observing,
    NotConfigured,
    Migrating,
    ReadyToRemoveLegacy,
    Verified,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub verified: bool,
    /// The top-level prefix this apiserver wrote with; `null` if unknown.
    #[serde(default)]
    pub writer_prefix: Option<String>,
    #[serde(default)]
    pub reads_by_prefix: BTreeMap<String, i64>,
    #[serde(default)]
    pub secrets_listed: i64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RewriteProgress {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub rewritten: i64,
    #[serde(default)]
    pub failed: i64,
}

/// Reads `status.phase` leniently: a phase this version does not know (every
/// v0.1.11 phase, such as `AwaitingKmsConfig`), `null`, or any other shape is
/// `Observing`. A strict read would make a leftover v0.1.11 object
/// undeserializable, the watcher would fail, and its finalizer would never be
/// stripped. `Observing` claims nothing; the phase is re-derived every reconcile.
fn lenient_phase<'de, D>(deserializer: D) -> Result<EncryptionPhase, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EtcdEncryptionStatus {
    #[serde(default, deserialize_with = "lenient_phase")]
    pub phase: EncryptionPhase,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub legacy_prefixes: Vec<String>,
    #[serde(default)]
    pub nodes: Vec<NodeStatus>,
    #[serde(default)]
    pub rewrite: RewriteProgress,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// The storage prefix of objects written through the target KMS v2 provider.
pub fn target_prefix(kms_provider_name: &str) -> String {
    format!("k8s:enc:kms:v2:{kms_provider_name}:")
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum EtcdEncryptionSpecError {
    #[error("spec.kmsProviderName must be set (a CR left over from v0.1.11 has no such field)")]
    EmptyKmsProviderName,
    #[error("spec.kmsProviderName {0:?} must not contain whitespace or ':'")]
    InvalidKmsProviderName(String),
}

impl EtcdEncryptionSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        "InvalidSpec"
    }
}

pub fn validate_etcd_encryption(spec: &EtcdEncryptionSpec) -> Result<(), EtcdEncryptionSpecError> {
    let name = &spec.kms_provider_name;
    if name.is_empty() {
        return Err(EtcdEncryptionSpecError::EmptyKmsProviderName);
    }
    if name.chars().any(|c| c.is_whitespace() || c == ':') {
        return Err(EtcdEncryptionSpecError::InvalidKmsProviderName(name.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec() -> EtcdEncryptionSpec {
        serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "kmsProviderName": "barbican"
        }))
        .expect("valid spec deserializes")
    }

    #[test]
    fn omitted_fields_default_to_observe_only() {
        let spec = valid_spec();

        assert_eq!(spec.rewrite, RewriteMode::Disabled);
        assert!(!spec.acknowledgements.legacy_providers_removed);
    }

    #[test]
    fn a_valid_spec_passes_validation() {
        assert_eq!(validate_etcd_encryption(&valid_spec()), Ok(()));
    }

    #[test]
    fn target_prefix_is_the_kms_v2_storage_prefix() {
        assert_eq!(target_prefix("barbican"), "k8s:enc:kms:v2:barbican:");
    }

    #[test]
    fn a_leftover_v0_1_11_spec_deserializes_but_is_rejected_by_validation() {
        // Review Focus 5: the old shape has no kmsProviderName. It must not break
        // the watcher (deserialization succeeds) and must be reported.
        let spec: EtcdEncryptionSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "barbican",
            "barbican": { "image": "img:1", "cloudConfigSecretRef": { "name": "cc" } },
            "acknowledgements": { "kmsConfigApplied": true }
        }))
        .expect("an old-shape spec still deserializes (unknown fields ignored)");

        let err = validate_etcd_encryption(&spec).unwrap_err();

        assert_eq!(err, EtcdEncryptionSpecError::EmptyKmsProviderName);
        assert_eq!(err.reason(), "InvalidSpec");
    }

    #[test]
    fn provider_names_with_whitespace_or_colons_are_rejected() {
        for bad in [" barbican", "barbican ", "bar bican", "bar:bican", ":"] {
            let mut spec = valid_spec();
            spec.kms_provider_name = bad.to_string();

            assert_eq!(
                validate_etcd_encryption(&spec),
                Err(EtcdEncryptionSpecError::InvalidKmsProviderName(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    /// A whole object as v0.1.11 stored it: old spec, the v0.1.11 finalizer,
    /// a pending deletion, and an old status carrying `phase`.
    fn v0_1_11_object(phase: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "EtcdEncryption",
            "metadata": {
                "name": "default",
                "uid": "0b5d6c1e-0000-4000-8000-000000000001",
                "resourceVersion": "12345",
                "generation": 4,
                "creationTimestamp": "2026-09-01T00:00:00Z",
                "deletionTimestamp": "2026-10-01T00:00:00Z",
                "deletionGracePeriodSeconds": 0,
                "finalizers": ["platform.rye.ninja/cleanup"]
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "barbican",
                "barbican": { "image": "registry.example/barbican-kms:1.0", "cloudConfigSecretRef": { "name": "cloud-config" } },
                "acknowledgements": {
                    "kmsConfigApplied": true,
                    "plaintextRemoved": false,
                    "kmsReverted": false,
                    "kmsRemoved": false
                }
            },
            "status": {
                "phase": phase,
                "observedGeneration": 4,
                "appliedResources": [
                    { "apiVersion": "apps/v1", "kind": "DaemonSet", "namespace": "kube-system", "name": "barbican-kms" }
                ],
                "conditions": [{
                    "type": "Ready",
                    "status": "False",
                    "reason": "AwaitingKmsConfig",
                    "message": "apply the Talos patch",
                    "lastTransitionTime": "2026-09-01T00:00:00Z",
                    "observedGeneration": 4
                }],
                "talosPatches": { "enableKms": "machine: {}", "removeIdentity": null, "revert": null, "removeKms": null },
                "patchGenerations": { "enableKms": 2, "removeIdentity": null, "revert": null, "removeKms": null },
                "rewrite": { "total": 12, "rewritten": 11, "failed": 1 }
            }
        })
    }

    #[test]
    fn a_whole_v0_1_11_object_deserializes_with_every_old_phase() {
        // Final review C1: an unknown old phase must not break the watcher,
        // or the v0.1.11 finalizer is never stripped and the delete hangs.
        for phase in [
            "Pending",
            "InstallingPlugin",
            "AwaitingKmsConfig",
            "Rewriting",
            "AwaitingPlaintextRemoval",
            "Encrypted",
            "RevertingKms",
            "Decrypting",
            "AwaitingKmsRemoval",
        ] {
            let obj: EtcdEncryption = serde_json::from_value(v0_1_11_object(phase))
                .unwrap_or_else(|e| panic!("v0.1.11 object with phase {phase} must deserialize: {e}"));

            let status = obj.status.as_ref().expect("status kept");
            assert_eq!(status.phase, EncryptionPhase::Observing, "{phase}");
            assert_eq!(status.rewrite, RewriteProgress { total: 12, rewritten: 11, failed: 1 });
            assert_eq!(validate_etcd_encryption(&obj.spec), Err(EtcdEncryptionSpecError::EmptyKmsProviderName));
            assert!(obj.metadata.deletion_timestamp.is_some());
            assert_eq!(
                crate::etcd_encryption_reconciler::strip_finalizer(obj.metadata.finalizers.as_deref().unwrap_or(&[])),
                Some(vec![]),
                "{phase}"
            );
        }
    }

    #[test]
    fn known_phases_still_deserialize_and_a_null_phase_is_observing() {
        for phase in ["Observing", "NotConfigured", "Migrating", "ReadyToRemoveLegacy", "Verified"] {
            let status: EtcdEncryptionStatus = serde_json::from_value(serde_json::json!({ "phase": phase })).unwrap();
            assert_eq!(serde_json::to_value(status.phase).unwrap(), phase);
        }
        let status: EtcdEncryptionStatus = serde_json::from_value(serde_json::json!({ "phase": null })).unwrap();
        assert_eq!(status.phase, EncryptionPhase::Observing);
        let status: EtcdEncryptionStatus = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(status.phase, EncryptionPhase::Observing);
    }

    #[test]
    fn status_counters_are_i64_and_nullable_fields_serialize_as_null() {
        let mut status = EtcdEncryptionStatus::default();
        status.nodes.push(NodeStatus {
            name: "cp1".to_string(),
            address: "10.0.0.1".to_string(),
            verified: false,
            writer_prefix: None,
            reads_by_prefix: Default::default(),
            secrets_listed: 3,
            reason: "x".to_string(),
        });

        let json = serde_json::to_value(&status).unwrap();

        assert_eq!(json["phase"], "Observing");
        assert_eq!(json["rewrite"]["total"], 0);
        assert_eq!(json["nodes"][0]["writerPrefix"], serde_json::Value::Null);
        assert_eq!(json["nodes"][0]["secretsListed"], 3);
    }
}
