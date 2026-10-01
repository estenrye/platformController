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

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EtcdEncryptionStatus {
    #[serde(default)]
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
