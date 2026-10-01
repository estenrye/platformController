use crate::crd::{AppliedResourceRef, Condition, PlatformKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    pub provider: KmsProviderKind,
    /// Settings for `provider: barbican`. Must be set when `provider` is
    /// `barbican`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barbican: Option<BarbicanSpec>,
    /// The operator's explicit gates in the apply-the-Talos-patch protocol.
    /// Always `false` on first apply; the controller never changes them.
    #[serde(default)]
    pub acknowledgements: Acknowledgements,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KmsProviderKind {
    Barbican,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BarbicanSpec {
    /// The `barbican-kms-plugin` image, pinned. No default: the controller
    /// does not choose a plugin version for you.
    pub image: String,
    /// A Secret in `kube-system` holding the whole `cloud.conf` under the key
    /// `cloud.conf` -- OpenStack credentials plus `[KeyManager] key-id`. The
    /// controller never reads it.
    pub cloud_config_secret_ref: SecretNameRef,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
pub struct SecretNameRef {
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Acknowledgements {
    /// Set true after applying the "enable KMS" Talos patch.
    #[serde(default)]
    pub kms_config_applied: bool,
    /// Set true after applying the "remove identity" Talos patch.
    #[serde(default)]
    pub plaintext_removed: bool,
    /// Deletion only: set true after applying the revert patch (identity
    /// first, kms second).
    #[serde(default)]
    pub kms_reverted: bool,
    /// Deletion only: set true after applying the remove-kms patch.
    #[serde(default)]
    pub kms_removed: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum EncryptionPhase {
    #[default]
    Pending,
    InstallingPlugin,
    AwaitingKmsConfig,
    Rewriting,
    AwaitingPlaintextRemoval,
    Encrypted,
    RevertingKms,
    Decrypting,
    AwaitingKmsRemoval,
}

/// Talos machine-config patches the operator applies. `None` until the
/// protocol reaches the step that needs them; serialized as `null` (not
/// skipped) so a merge-patch status write can clear one.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TalosPatches {
    #[serde(default)]
    pub enable_kms: Option<String>,
    #[serde(default)]
    pub remove_identity: Option<String>,
    #[serde(default)]
    pub revert: Option<String>,
    #[serde(default)]
    pub remove_kms: Option<String>,
}

/// `metadata.generation` at the status write that FIRST published each Talos
/// patch. Set once (only while `None`) and never overwritten. An
/// acknowledgement only counts if the object's generation is strictly greater:
/// the ack must have been set after the patch it acknowledges was visible.
/// Serialized as `null` when unset, like `TalosPatches`.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PatchGenerations {
    #[serde(default)]
    pub enable_kms: Option<i64>,
    #[serde(default)]
    pub remove_identity: Option<i64>,
    #[serde(default)]
    pub revert: Option<i64>,
    #[serde(default)]
    pub remove_kms: Option<i64>,
}

/// The acknowledgements that count. Each is true only when the raw ack is true
/// AND its patch was published at a generation strictly below `generation`.
/// An ack that was already true when its patch was first published (same
/// generation) does not count; the operator must flip it false then true
/// (any later spec change bumps the generation) after applying the patch.
pub fn effective_acks(acks: Acknowledgements, generation: i64, gens: &PatchGenerations) -> Acknowledgements {
    let after = |published: Option<i64>| published.is_some_and(|g| generation > g);
    Acknowledgements {
        kms_config_applied: acks.kms_config_applied && after(gens.enable_kms),
        plaintext_removed: acks.plaintext_removed && after(gens.remove_identity),
        kms_reverted: acks.kms_reverted && after(gens.revert),
        kms_removed: acks.kms_removed && after(gens.remove_kms),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RewriteProgress {
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub rewritten: u64,
    #[serde(default)]
    pub failed: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EtcdEncryptionStatus {
    #[serde(default)]
    pub phase: EncryptionPhase,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub applied_resources: Vec<AppliedResourceRef>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub talos_patches: TalosPatches,
    /// The generation at which each patch in `talosPatches` was first
    /// published; see `effective_acks`.
    #[serde(default)]
    pub patch_generations: PatchGenerations,
    #[serde(default)]
    pub rewrite: RewriteProgress,
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum EtcdEncryptionSpecError {
    #[error("spec.provider is barbican but spec.barbican is not set")]
    MissingProviderBlock,
    #[error("spec.barbican.image must not be empty")]
    EmptyImage,
    #[error("spec.barbican.image {0:?} has leading or trailing whitespace")]
    ImageHasWhitespace(String),
    #[error("spec.barbican.cloudConfigSecretRef.name must not be empty")]
    EmptySecretName,
    #[error(
        "spec.acknowledgements are out of order: plaintextRemoved and kmsReverted require \
         kmsConfigApplied, and kmsRemoved requires kmsReverted"
    )]
    AcknowledgementsOutOfOrder,
}

impl EtcdEncryptionSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            EtcdEncryptionSpecError::MissingProviderBlock
            | EtcdEncryptionSpecError::EmptyImage
            | EtcdEncryptionSpecError::ImageHasWhitespace(_)
            | EtcdEncryptionSpecError::EmptySecretName => "InvalidSpec",
            EtcdEncryptionSpecError::AcknowledgementsOutOfOrder => "InvalidAcknowledgements",
        }
    }
}

pub fn validate_etcd_encryption(spec: &EtcdEncryptionSpec) -> Result<(), EtcdEncryptionSpecError> {
    match spec.provider {
        KmsProviderKind::Barbican => {
            let barbican = spec.barbican.as_ref().ok_or(EtcdEncryptionSpecError::MissingProviderBlock)?;
            if barbican.image.trim().is_empty() {
                return Err(EtcdEncryptionSpecError::EmptyImage);
            }
            if barbican.image.trim() != barbican.image {
                return Err(EtcdEncryptionSpecError::ImageHasWhitespace(barbican.image.clone()));
            }
            if barbican.cloud_config_secret_ref.name.trim().is_empty() {
                return Err(EtcdEncryptionSpecError::EmptySecretName);
            }
        }
    }
    let acks = spec.acknowledgements;
    let in_order = (!acks.plaintext_removed || acks.kms_config_applied)
        && (!acks.kms_reverted || acks.kms_config_applied)
        && (!acks.kms_removed || acks.kms_reverted);
    if !in_order {
        return Err(EtcdEncryptionSpecError::AcknowledgementsOutOfOrder);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec() -> EtcdEncryptionSpec {
        serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "barbican",
            "barbican": {
                "image": "registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0",
                "cloudConfigSecretRef": { "name": "barbican-kms-cloud-config" }
            }
        }))
        .expect("valid spec deserializes")
    }

    #[test]
    fn acknowledgements_default_to_false_when_omitted() {
        let spec = valid_spec();

        assert_eq!(spec.acknowledgements, Acknowledgements::default());
        assert!(!spec.acknowledgements.kms_config_applied);
    }

    #[test]
    fn a_valid_spec_passes_validation() {
        assert_eq!(validate_etcd_encryption(&valid_spec()), Ok(()));
    }

    #[test]
    fn provider_without_its_block_is_rejected() {
        let mut spec = valid_spec();
        spec.barbican = None;

        let err = validate_etcd_encryption(&spec).unwrap_err();

        assert_eq!(err, EtcdEncryptionSpecError::MissingProviderBlock);
        assert_eq!(err.reason(), "InvalidSpec");
    }

    #[test]
    fn empty_or_padded_image_is_rejected() {
        let mut spec = valid_spec();
        spec.barbican.as_mut().unwrap().image = "  ".to_string();
        assert_eq!(validate_etcd_encryption(&spec), Err(EtcdEncryptionSpecError::EmptyImage));

        spec.barbican.as_mut().unwrap().image = " img:1".to_string();
        assert!(matches!(
            validate_etcd_encryption(&spec),
            Err(EtcdEncryptionSpecError::ImageHasWhitespace(_))
        ));
    }

    #[test]
    fn empty_cloud_config_secret_name_is_rejected() {
        let mut spec = valid_spec();
        spec.barbican.as_mut().unwrap().cloud_config_secret_ref.name = String::new();

        assert_eq!(validate_etcd_encryption(&spec), Err(EtcdEncryptionSpecError::EmptySecretName));
    }

    #[test]
    fn acknowledgements_out_of_order_are_rejected() {
        // Review Focus 4: plaintextRemoved before kmsConfigApplied, and so on.
        let cases = [
            Acknowledgements { plaintext_removed: true, ..Default::default() },
            Acknowledgements { kms_reverted: true, ..Default::default() },
            Acknowledgements { kms_config_applied: true, kms_removed: true, ..Default::default() },
        ];
        for acknowledgements in cases {
            let mut spec = valid_spec();
            spec.acknowledgements = acknowledgements;

            let err = validate_etcd_encryption(&spec).unwrap_err();

            assert_eq!(err, EtcdEncryptionSpecError::AcknowledgementsOutOfOrder, "{acknowledgements:?}");
            assert_eq!(err.reason(), "InvalidAcknowledgements");
        }
    }

    #[test]
    fn acknowledgements_in_order_are_accepted() {
        let mut spec = valid_spec();
        spec.acknowledgements = Acknowledgements {
            kms_config_applied: true,
            plaintext_removed: true,
            kms_reverted: true,
            kms_removed: true,
        };

        assert_eq!(validate_etcd_encryption(&spec), Ok(()));
    }

    #[test]
    fn status_serializes_unset_patches_as_null_so_a_merge_patch_clears_them() {
        let json = serde_json::to_value(EtcdEncryptionStatus::default()).unwrap();

        assert_eq!(json["talosPatches"]["enableKms"], serde_json::Value::Null);
        assert_eq!(json["patchGenerations"]["enableKms"], serde_json::Value::Null);
        assert_eq!(json["phase"], "Pending");
    }

    #[test]
    fn a_status_without_patch_generations_deserializes_to_none() {
        let status: EtcdEncryptionStatus = serde_json::from_value(serde_json::json!({ "phase": "Encrypted" })).unwrap();

        assert_eq!(status.patch_generations, PatchGenerations::default());
    }

    const ALL_ACKS: Acknowledgements =
        Acknowledgements { kms_config_applied: true, plaintext_removed: true, kms_reverted: true, kms_removed: true };

    #[test]
    fn an_ack_for_an_unpublished_patch_is_never_effective() {
        assert_eq!(effective_acks(ALL_ACKS, 99, &PatchGenerations::default()), Acknowledgements::default());
    }

    #[test]
    fn an_ack_already_set_when_its_patch_was_published_is_not_effective() {
        // The patch was published by the write at generation 4, which already
        // carried the ack: the operator cannot have applied the patch first.
        let gens = PatchGenerations { revert: Some(4), ..Default::default() };

        assert!(!effective_acks(ALL_ACKS, 4, &gens).kms_reverted);
    }

    #[test]
    fn an_ack_set_at_a_later_generation_is_effective() {
        let gens = PatchGenerations { enable_kms: Some(2), remove_identity: Some(3), revert: Some(4), remove_kms: Some(5) };

        assert_eq!(effective_acks(ALL_ACKS, 6, &gens), ALL_ACKS);
        let partial = effective_acks(ALL_ACKS, 4, &gens);
        assert_eq!(
            partial,
            Acknowledgements { kms_config_applied: true, plaintext_removed: true, kms_reverted: false, kms_removed: false }
        );
    }

    #[test]
    fn flipping_a_preset_ack_after_publication_makes_it_count() {
        let gens = PatchGenerations { enable_kms: Some(2), ..Default::default() };
        let set = Acknowledgements { kms_config_applied: true, ..Default::default() };

        // Generation 2: published while already true -> not effective.
        assert!(!effective_acks(set, 2, &gens).kms_config_applied);
        // Generation 3: flipped false -> not effective (the raw value is false).
        assert!(!effective_acks(Acknowledgements::default(), 3, &gens).kms_config_applied);
        // Generation 4: flipped back true -> effective.
        assert!(effective_acks(set, 4, &gens).kms_config_applied);
    }

    #[test]
    fn a_false_ack_is_never_made_effective() {
        let gens = PatchGenerations { enable_kms: Some(1), remove_identity: Some(1), revert: Some(1), remove_kms: Some(1) };

        assert_eq!(effective_acks(Acknowledgements::default(), 9, &gens), Acknowledgements::default());
    }
}
