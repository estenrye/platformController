# EtcdEncryption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a seventh platform component, an `EtcdEncryption` custom resource, that installs the OpenStack Barbican KMS plugin, publishes the Talos machine-config patches an operator applies, re-encrypts every Secret through KMS, and verifies the cluster reaches "no plaintext Secrets in etcd". The core is provider-agnostic so Azure, AWS, GCP and OCI can be added later as one builder plus one enum variant each.

**Architecture:** A new cluster-scoped singleton CRD with its own reconciler under the same leader lease as the other six. The reconciler is a thin glue layer over four pure, independently tested units: a phase state machine (`encryption_phase.rs`), a Talos-patch generator (`talos_patches.rs`), a provider plan builder (`kms_provider.rs` + `kms_barbican.rs`), and a Secret-rewrite loop behind a `SecretStore` trait so it is testable without a cluster (`secret_rewrite.rs`). The controller never holds a Talos credential: the operator applies the patches and acknowledges each step in `spec.acknowledgements`; the controller also probes the apiserver (metrics + a canary Secret) and advances only when both agree.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, `serde_yaml`, `thiserror`, `tokio`. One new dependency: `http = "1"` (to issue the raw `GET /metrics` request through `kube::Client::request_text`).

**Spec:** [docs/superpowers/specs/2026-09-30-etcd-encryption-design.md](../specs/2026-09-30-etcd-encryption-design.md) — **Task 1 amends it first** (two corrections found while planning, listed under Global Constraints). Executors read the amended spec.

## Global Constraints

Every task's requirements implicitly include this section.

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `EtcdEncryption`, **cluster-scoped**, shortname `etcdenc`, plural `etcdencryptions` (kube-derive default), singleton named `default`.
- `platformKind` accepts only `talos-linux`. `provider` is an enum with one variant today, `barbican` (serde lowercase); the provider's settings live in a sibling block named after it (`spec.barbican`), exactly like `CloudControllerManager`. Provider and block must match, else `InvalidSpec`.
- `spec.barbican.image` is required, pinned, no default, leading/trailing whitespace rejected. `spec.barbican.cloudConfigSecretRef.name` names a Secret in `kube-system` whose key `cloud.conf` holds the OpenStack credentials **and** the `[KeyManager] key-id`. The controller never reads that Secret.
- **Spec correction 1 (found while planning):** there is **no `spec.barbican.keyId`**. The upstream plugin reads the key ID only from `cloud.conf` (`[KeyManager] key-id`); the controller cannot see it and a second copy on the CR could only contradict it. The spec's Open item 3 is resolved by removal.
- **Spec correction 2 (found while planning):** the deletion path is **three** operator steps, not one. Reverting straight to `identity`-only would leave every KMS-encrypted Secret unreadable. The correct order is: (a) patch with `identity` **first** and `kms` still listed (new writes go plaintext, KMS still reads old ones), then rewrite all Secrets; (b) patch removing `kms` entirely; (c) only then remove the plugin. This adds one acknowledgement, `kmsRemoved`, and three deletion-time phases.
- Acknowledgements (`spec.acknowledgements`, all default `false`): `kmsConfigApplied`, `plaintextRemoved`, `kmsReverted`, `kmsRemoved`. Ordering is validated: `plaintextRemoved` requires `kmsConfigApplied`; `kmsReverted` requires `kmsConfigApplied`; `kmsRemoved` requires `kmsReverted`. Violations are `InvalidAcknowledgements`.
- Phases (`status.phase`), forward: `Pending → InstallingPlugin → AwaitingKmsConfig → Rewriting → AwaitingPlaintextRemoval → Encrypted`. Deletion-time: `RevertingKms → Decrypting → AwaitingKmsRemoval`. **There is no `Failed` phase**: protocol position must survive transient failures, so failures are a `Ready=False` condition with a reason; the phase never regresses and acknowledgements are never reset.
- The plugin `DaemonSet` is built in Rust (no chart, no upstream fetch), in `kube-system`, named `barbican-kms`, label `k8s-app: barbican-kms`, `hostNetwork: true`, `dnsPolicy: Default`, `nodeSelector: node-role.kubernetes.io/control-plane: ""`, tolerating `node-role.kubernetes.io/control-plane` / `node-role.kubernetes.io/master` (`NoSchedule`) and `node.cloudprovider.kubernetes.io/uninitialized` (`NoSchedule`), mounting the credentials Secret at `/etc/config` and `hostPath /var/lib/kms` (type `DirectoryOrCreate`) at `/kms/`, args `/bin/barbican-kms-plugin --socketpath=/kms/kms.sock --cloud-config=/etc/config/cloud.conf` (taken from upstream `manifests/barbican-kms/ds.yaml`, fetched 2026-09-30). Host socket path: `/var/lib/kms/kms.sock`. No `serviceAccountName` (the plugin does not call the Kubernetes API).
- KMS provider block in the `EncryptionConfiguration`: `kms: {apiVersion: v2, name: barbican, endpoint: unix:///var/lib/kms/kms.sock, timeout: 3s}`.
- The controller speaks only the Kubernetes API. It never applies Talos config.
- Cleanup must **never return `Ok`** while the plugin must stay: `kube::runtime::finalizer` strips the finalizer on any `Ok` from the Cleanup arm. Waiting states return an `Err`.
- Every networked poll uses `tokio::time::timeout_at`, never a bare `.await` (see `docs/memory/wait-for-crd-established.md`).
- The rewrite never logs Secret data; failures are reported by namespace/name only.
- Chart-less: this component must not shell out to `helm`.
- `cargo test` and `cargo clippy --all-targets` must pass (the CI commands); `deploy/crd.yaml` must equal `crds::generated_yaml()`.

## Review Focus

The inputs and failure modes the spec implies that most plausibly bite someone using this. The task named in brackets owns the test that pins each.

1. **A Secret deleted, or changed by someone else, mid-rewrite** must not fail the run: deleted counts as done, a 409 retries (bounded). [Task 5]
2. **An admission webhook rejects the no-op update of one Secret.** That Secret is counted failed and reported by name; the rest still get rewritten; the phase must not complete while any failed. [Task 5, Task 7]
3. **Plugin Ready on fewer nodes than there are control-plane nodes** (or zero nodes visible) must not publish Talos patch 1: a rolling apiserver restart would hit a missing socket. [Task 7]
4. **Operator sets `plaintextRemoved` (or `kmsRemoved`) before the earlier acknowledgement.** Rejected `InvalidAcknowledgements`, never silently honoured. [Task 1]
5. **Deleting the CR after patch 1 was acknowledged.** The finalizer must keep returning `Err` (never `Ok`) until the plugin is safe to remove, and the revert patch must list `identity` *before* `kms`. [Task 3, Task 4, Task 7]

---

### Task 1: Amend the spec, add the CRD types, validation and CRD manifest

**Files:**
- Modify: `docs/superpowers/specs/2026-09-30-etcd-encryption-design.md`
- Create: `src/etcd_encryption.rs`
- Modify: `src/lib.rs`, `src/crds.rs`, `deploy/crd.yaml` (regenerated), `deploy/README.md`, `tests/bootstrap_manifests.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, PlatformKind}`.
- Produces (used by every later task), in `src/etcd_encryption.rs`:
  - `pub struct EtcdEncryptionSpec { pub platform_kind: PlatformKind, pub provider: KmsProviderKind, pub barbican: Option<BarbicanSpec>, pub acknowledgements: Acknowledgements }` (CRD kind `EtcdEncryption`)
  - `pub enum KmsProviderKind { Barbican }`
  - `pub struct BarbicanSpec { pub image: String, pub cloud_config_secret_ref: SecretNameRef }`
  - `pub struct SecretNameRef { pub name: String }`
  - `pub struct Acknowledgements { pub kms_config_applied: bool, pub plaintext_removed: bool, pub kms_reverted: bool, pub kms_removed: bool }` (`Default` = all false, `Copy`)
  - `pub enum EncryptionPhase { Pending, InstallingPlugin, AwaitingKmsConfig, Rewriting, AwaitingPlaintextRemoval, Encrypted, RevertingKms, Decrypting, AwaitingKmsRemoval }` (`Default` = `Pending`, `Copy`)
  - `pub struct EtcdEncryptionStatus { pub phase: EncryptionPhase, pub observed_generation: i64, pub applied_resources: Vec<AppliedResourceRef>, pub conditions: Vec<Condition>, pub talos_patches: TalosPatches, pub rewrite: RewriteProgress }`
  - `pub struct TalosPatches { pub enable_kms: Option<String>, pub remove_identity: Option<String>, pub revert: Option<String>, pub remove_kms: Option<String> }`
  - `pub struct RewriteProgress { pub total: u64, pub rewritten: u64, pub failed: u64 }`
  - `pub enum EtcdEncryptionSpecError` with `pub fn reason(&self) -> &'static str`
  - `pub fn validate_etcd_encryption(spec: &EtcdEncryptionSpec) -> Result<(), EtcdEncryptionSpecError>`

- [ ] **Step 1: Amend the spec**

Make these edits to `docs/superpowers/specs/2026-09-30-etcd-encryption-design.md`:

1. In the API YAML block, delete the line `keyId: <barbican-key-uuid>    # required; ...`, add `kmsRemoved: false       # set true after applying the remove-kms patch (deletion only)` under `kmsReverted`, and replace the `cloudConfigSecretRef` bullet's parenthetical about `keyId` (the sentence starting "`keyId` is on the spec because") with: "There is no `keyId` field: the plugin reads the key only from `cloud.conf`'s `[KeyManager] key-id`, which the controller never reads, so a copy on the CR could only contradict it."
2. Replace the whole **Deletion** section's second bullet with:

   > - **After patch 1 is acknowledged** (any later phase): the CR stays `Terminating` and the controller walks three steps, never returning success from the finalizer until the plugin is safe to remove. (1) `RevertingKms`: publish `status.talosPatches.revert`, a patch listing `identity` **first** and `kms` second, so new writes are plaintext while KMS can still read old ciphertext; wait for `kmsReverted`. (2) `Decrypting`: rewrite every Secret; then `AwaitingKmsRemoval`: publish `status.talosPatches.removeKms` (identity only) and wait for `kmsRemoved` **and** a probe showing no KMS provider active. (3) Remove the DaemonSet. Going straight to `identity`-only would make every KMS-encrypted Secret unreadable.

3. In the **Phases** section add a sentence after the numbered list: "Deletion adds three phases: `RevertingKms`, `Decrypting`, `AwaitingKmsRemoval`. There is no `Failed` phase: failures are a `Ready=False` condition with a reason, so the protocol position survives them."
4. In **Open verification items**, delete item 3's `keyId` clause so it reads: "The Barbican plugin's image reference and tag."

- [ ] **Step 2: Write the failing tests**

Create `src/etcd_encryption.rs` containing only a `#[cfg(test)] mod tests` with the tests below (the types do not exist yet, so it will not compile):

```rust
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
        assert_eq!(json["phase"], "Pending");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib etcd_encryption 2>&1 | tail -15`
Expected: FAIL to compile — `cannot find type EtcdEncryptionSpec` (and others).

- [ ] **Step 4: Write the implementation**

Put this at the top of `src/etcd_encryption.rs` (above the tests module):

```rust
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
```

Then register it:
- `src/lib.rs`: add `pub mod etcd_encryption;` (keep the list's existing style; position does not matter).
- `src/crds.rs`: add `crate::etcd_encryption::EtcdEncryption::crd(),` as the last element of the array.
- `tests/bootstrap_manifests.rs`: in the test whose expected list ends `"snapshotcontrollers.platform.rye.ninja",` (around line 85), append `"etcdencryptions.platform.rye.ninja",`.
- `deploy/README.md`: after the line `kubectl wait --for=condition=established --timeout=60s crd/snapshotcontrollers.platform.rye.ninja` add `kubectl wait --for=condition=established --timeout=60s crd/etcdencryptions.platform.rye.ninja`.

- [ ] **Step 5: Regenerate the CRD manifest and run the tests**

Run: `cargo run -q --bin crdgen > deploy/crd.yaml && cargo test --lib etcd_encryption && cargo test --test bootstrap_manifests`
Expected: all PASS, including `crd_yaml_matches_the_generated_crds`.

- [ ] **Step 6: Commit**

```bash
git add docs/superpowers/specs/2026-09-30-etcd-encryption-design.md src/etcd_encryption.rs src/lib.rs src/crds.rs deploy/crd.yaml deploy/README.md tests/bootstrap_manifests.rs
git commit -m "feat: EtcdEncryption CRD types and validation; amend spec

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Provider plan — KmsPlan and the Barbican DaemonSet builder

**Files:**
- Create: `src/kms_provider.rs`, `src/kms_barbican.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `EtcdEncryptionSpec`, `KmsProviderKind`, `BarbicanSpec`, `EtcdEncryptionSpecError` (Task 1).
- Produces, in `src/kms_provider.rs`:
  - `pub const KMS_SOCKET_DIR: &str = "/var/lib/kms";`
  - `pub const PLUGIN_NAMESPACE: &str = "kube-system";`
  - `pub struct KmsPlan { pub provider_name: &'static str, pub socket_path: String, pub daemonset_name: String, pub daemonset: kube::api::DynamicObject }`
  - `impl KmsPlan { pub fn provider_block(&self) -> serde_json::Value }` → `{"kms": {"apiVersion":"v2","name":<provider_name>,"endpoint":"unix://<socket_path>","timeout":"3s"}}`
  - `pub fn kms_plan(spec: &EtcdEncryptionSpec) -> Result<KmsPlan, EtcdEncryptionSpecError>`
- Produces, in `src/kms_barbican.rs`: `pub fn plan(spec: &BarbicanSpec) -> KmsPlan`.

- [ ] **Step 1: Write the failing tests**

`src/kms_barbican.rs` (tests only for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::SecretNameRef;

    fn spec() -> BarbicanSpec {
        BarbicanSpec {
            image: "registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0".to_string(),
            cloud_config_secret_ref: SecretNameRef { name: "barbican-kms-cloud-config".to_string() },
        }
    }

    #[test]
    fn plan_names_the_socket_and_daemonset() {
        let plan = plan(&spec());

        assert_eq!(plan.provider_name, "barbican");
        assert_eq!(plan.socket_path, "/var/lib/kms/kms.sock");
        assert_eq!(plan.daemonset_name, "barbican-kms");
    }

    #[test]
    fn daemonset_is_a_hostnetwork_control_plane_daemonset_in_kube_system() {
        let object = plan(&spec()).daemonset;
        let types = object.types.as_ref().unwrap();

        assert_eq!((types.api_version.as_str(), types.kind.as_str()), ("apps/v1", "DaemonSet"));
        assert_eq!(object.metadata.name.as_deref(), Some("barbican-kms"));
        assert_eq!(object.metadata.namespace.as_deref(), Some("kube-system"));
        let pod = &object.data["spec"]["template"]["spec"];
        assert_eq!(pod["hostNetwork"], true);
        assert_eq!(pod["dnsPolicy"], "Default");
        assert_eq!(pod["nodeSelector"]["node-role.kubernetes.io/control-plane"], "");
        assert!(pod.get("serviceAccountName").is_none());
    }

    #[test]
    fn daemonset_tolerates_control_plane_and_uninitialized_taints() {
        let object = plan(&spec()).daemonset;
        let tolerations = object.data["spec"]["template"]["spec"]["tolerations"].as_array().unwrap();
        let keys: Vec<&str> = tolerations.iter().map(|t| t["key"].as_str().unwrap()).collect();

        assert!(keys.contains(&"node-role.kubernetes.io/control-plane"));
        assert!(keys.contains(&"node-role.kubernetes.io/master"));
        assert!(keys.contains(&"node.cloudprovider.kubernetes.io/uninitialized"));
    }

    #[test]
    fn container_uses_the_pinned_image_and_upstream_args() {
        let object = plan(&spec()).daemonset;
        let container = &object.data["spec"]["template"]["spec"]["containers"][0];

        assert_eq!(container["image"], "registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0");
        assert_eq!(
            container["args"],
            serde_json::json!([
                "/bin/barbican-kms-plugin",
                "--socketpath=/kms/kms.sock",
                "--cloud-config=/etc/config/cloud.conf"
            ])
        );
    }

    #[test]
    fn volumes_mount_the_credentials_secret_and_the_host_socket_dir() {
        let object = plan(&spec()).daemonset;
        let volumes = object.data["spec"]["template"]["spec"]["volumes"].as_array().unwrap();

        let secret = volumes.iter().find(|v| v["name"] == "cloud-config-volume").unwrap();
        assert_eq!(secret["secret"]["secretName"], "barbican-kms-cloud-config");
        let socket = volumes.iter().find(|v| v["name"] == "socket-dir").unwrap();
        assert_eq!(socket["hostPath"]["path"], "/var/lib/kms/");
        assert_eq!(socket["hostPath"]["type"], "DirectoryOrCreate");
    }
}
```

`src/kms_provider.rs` (tests only for now):

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib kms_ 2>&1 | tail -10` (after adding `pub mod kms_provider; pub mod kms_barbican;` to `src/lib.rs`)
Expected: FAIL to compile — `cannot find function plan` / `kms_plan`.

- [ ] **Step 3: Implement**

Prepend to `src/kms_provider.rs`:

```rust
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
```

Prepend to `src/kms_barbican.rs`:

```rust
use crate::etcd_encryption::BarbicanSpec;
use crate::kms_provider::{KmsPlan, KMS_SOCKET_DIR, PLUGIN_NAMESPACE};
use kube::api::DynamicObject;

const DAEMONSET_NAME: &str = "barbican-kms";

/// The Barbican KMS plugin as a DaemonSet on the control-plane nodes, built
/// from typed fields. Shape taken from upstream
/// `manifests/barbican-kms/ds.yaml` (2026-09-30), minus its
/// `serviceAccountName` (the plugin never calls the Kubernetes API) and plus
/// `dnsPolicy: Default` (no cluster-DNS dependence on the bootstrap path).
pub fn plan(spec: &BarbicanSpec) -> KmsPlan {
    let daemonset: DynamicObject = serde_json::from_value(serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "DaemonSet",
        "metadata": {
            "name": DAEMONSET_NAME,
            "namespace": PLUGIN_NAMESPACE,
            "labels": { "k8s-app": DAEMONSET_NAME },
        },
        "spec": {
            "selector": { "matchLabels": { "k8s-app": DAEMONSET_NAME } },
            "updateStrategy": { "type": "RollingUpdate" },
            "template": {
                "metadata": { "labels": { "k8s-app": DAEMONSET_NAME } },
                "spec": {
                    "hostNetwork": true,
                    "dnsPolicy": "Default",
                    "nodeSelector": { "node-role.kubernetes.io/control-plane": "" },
                    "tolerations": [
                        { "key": "node.cloudprovider.kubernetes.io/uninitialized", "operator": "Exists", "effect": "NoSchedule" },
                        { "key": "node-role.kubernetes.io/master", "effect": "NoSchedule" },
                        { "key": "node-role.kubernetes.io/control-plane", "effect": "NoSchedule" },
                    ],
                    "containers": [{
                        "name": DAEMONSET_NAME,
                        "image": spec.image,
                        "args": [
                            "/bin/barbican-kms-plugin",
                            "--socketpath=/kms/kms.sock",
                            "--cloud-config=/etc/config/cloud.conf",
                        ],
                        "volumeMounts": [
                            { "name": "cloud-config-volume", "mountPath": "/etc/config" },
                            { "name": "socket-dir", "mountPath": "/kms/" },
                        ],
                        "livenessProbe": {
                            "exec": { "command": ["ls", "/kms/kms.sock"] },
                            "failureThreshold": 5,
                            "initialDelaySeconds": 10,
                            "timeoutSeconds": 10,
                            "periodSeconds": 60,
                        },
                    }],
                    "volumes": [
                        { "name": "cloud-config-volume", "secret": { "secretName": spec.cloud_config_secret_ref.name } },
                        { "name": "socket-dir", "hostPath": { "path": format!("{KMS_SOCKET_DIR}/"), "type": "DirectoryOrCreate" } },
                    ],
                },
            },
        },
    }))
    .expect("static DaemonSet JSON deserializes into a DynamicObject");

    KmsPlan {
        provider_name: "barbican",
        socket_path: format!("{KMS_SOCKET_DIR}/kms.sock"),
        daemonset_name: DAEMONSET_NAME.to_string(),
        daemonset,
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib kms_`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/kms_provider.rs src/kms_barbican.rs src/lib.rs
git commit -m "feat: KMS provider plan and Barbican plugin DaemonSet builder

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Talos patch generator

**Files:**
- Create: `src/talos_patches.rs`
- Modify: `src/lib.rs` (`pub mod talos_patches;`)

**Interfaces:**
- Consumes: `KmsPlan` + `KMS_SOCKET_DIR` (Task 2).
- Produces (all return a two-document YAML string: a v1alpha1 `cluster.apiServer.extraVolumes` patch, then a `KubeEtcdEncryptionConfig` document):
  - `pub fn enable_kms(plan: &KmsPlan) -> String` — providers `[kms, identity]`
  - `pub fn remove_identity(plan: &KmsPlan) -> String` — providers `[kms]`
  - `pub fn revert(plan: &KmsPlan) -> String` — providers `[identity, kms]` (identity **first**)
  - `pub fn remove_kms() -> String` — providers `[identity]`, and omits the volume document

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib talos_patches 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find function enable_kms`.

- [ ] **Step 3: Implement**

Prepend to `src/talos_patches.rs`:

```rust
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib talos_patches`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/talos_patches.rs src/lib.rs
git commit -m "feat: generate the Talos patches for each EtcdEncryption step

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Phase state machine

**Files:**
- Create: `src/encryption_phase.rs`
- Modify: `src/lib.rs` (`pub mod encryption_phase;`)

**Interfaces:**
- Consumes: `EncryptionPhase`, `Acknowledgements` (Task 1).
- Produces:
  - `pub struct PhaseInputs { pub current: EncryptionPhase, pub acks: Acknowledgements, pub plugin_ready: bool, pub kms_active: bool, pub canary_ok: bool, pub rewrite_complete: bool }`
  - `pub fn next_phase(inputs: &PhaseInputs) -> EncryptionPhase` — one forward step; deletion-time phases are returned unchanged.
  - `pub struct CleanupInputs { pub phase: EncryptionPhase, pub acks: Acknowledgements, pub kms_active: bool }`
  - `pub enum CleanupStep { RemovePlugin, AwaitRevertAck, Decrypt, AwaitKmsRemoval }`
  - `pub fn cleanup_step(inputs: &CleanupInputs) -> CleanupStep`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use EncryptionPhase::*;

    fn inputs(current: EncryptionPhase) -> PhaseInputs {
        PhaseInputs {
            current,
            acks: Acknowledgements::default(),
            plugin_ready: false,
            kms_active: false,
            canary_ok: false,
            rewrite_complete: false,
        }
    }

    #[test]
    fn pending_always_moves_to_installing_the_plugin() {
        assert_eq!(next_phase(&inputs(Pending)), InstallingPlugin);
    }

    #[test]
    fn installing_waits_for_the_plugin_on_every_control_plane_node() {
        assert_eq!(next_phase(&inputs(InstallingPlugin)), InstallingPlugin);
        let ready = PhaseInputs { plugin_ready: true, ..inputs(InstallingPlugin) };
        assert_eq!(next_phase(&ready), AwaitingKmsConfig);
    }

    #[test]
    fn awaiting_kms_config_needs_both_the_probe_and_the_acknowledgement() {
        let only_probe = PhaseInputs { kms_active: true, ..inputs(AwaitingKmsConfig) };
        assert_eq!(next_phase(&only_probe), AwaitingKmsConfig);

        let only_ack = PhaseInputs {
            acks: Acknowledgements { kms_config_applied: true, ..Default::default() },
            ..inputs(AwaitingKmsConfig)
        };
        assert_eq!(next_phase(&only_ack), AwaitingKmsConfig);

        let both = PhaseInputs { kms_active: true, ..only_ack };
        assert_eq!(next_phase(&both), Rewriting);
    }

    #[test]
    fn rewriting_completes_only_when_every_secret_was_rewritten() {
        assert_eq!(next_phase(&inputs(Rewriting)), Rewriting);
        let done = PhaseInputs { rewrite_complete: true, ..inputs(Rewriting) };
        assert_eq!(next_phase(&done), AwaitingPlaintextRemoval);
    }

    #[test]
    fn plaintext_removal_needs_ack_probe_and_canary() {
        let acks = Acknowledgements { kms_config_applied: true, plaintext_removed: true, ..Default::default() };
        let base = PhaseInputs { acks, ..inputs(AwaitingPlaintextRemoval) };

        assert_eq!(next_phase(&base), AwaitingPlaintextRemoval);
        assert_eq!(next_phase(&PhaseInputs { kms_active: true, ..base }), AwaitingPlaintextRemoval);
        assert_eq!(next_phase(&PhaseInputs { canary_ok: true, ..base }), AwaitingPlaintextRemoval);
        assert_eq!(next_phase(&PhaseInputs { kms_active: true, canary_ok: true, ..base }), Encrypted);
        let no_ack = PhaseInputs { kms_active: true, canary_ok: true, acks: Acknowledgements::default(), ..base };
        assert_eq!(next_phase(&no_ack), AwaitingPlaintextRemoval);
    }

    #[test]
    fn the_phase_never_regresses_when_probes_go_negative() {
        let degraded = PhaseInputs { plugin_ready: false, kms_active: false, canary_ok: false, ..inputs(Encrypted) };
        assert_eq!(next_phase(&degraded), Encrypted);
        let degraded = PhaseInputs { kms_active: false, ..inputs(Rewriting) };
        assert_eq!(next_phase(&degraded), Rewriting);
    }

    #[test]
    fn deletion_phases_are_not_advanced_by_the_forward_machine() {
        for phase in [RevertingKms, Decrypting, AwaitingKmsRemoval] {
            let all_true = PhaseInputs {
                acks: Acknowledgements { kms_config_applied: true, plaintext_removed: true, kms_reverted: true, kms_removed: true },
                plugin_ready: true,
                kms_active: true,
                canary_ok: true,
                rewrite_complete: true,
                ..inputs(phase)
            };
            assert_eq!(next_phase(&all_true), phase);
        }
    }

    fn cleanup(phase: EncryptionPhase, acks: Acknowledgements, kms_active: bool) -> CleanupStep {
        cleanup_step(&CleanupInputs { phase, acks, kms_active })
    }

    #[test]
    fn nothing_depends_on_the_plugin_before_patch_1_is_acknowledged() {
        let none = Acknowledgements::default();
        assert_eq!(cleanup(Pending, none, false), CleanupStep::RemovePlugin);
        assert_eq!(cleanup(InstallingPlugin, none, false), CleanupStep::RemovePlugin);
        assert_eq!(cleanup(AwaitingKmsConfig, none, false), CleanupStep::RemovePlugin);
    }

    #[test]
    fn after_patch_1_is_acknowledged_the_plugin_must_stay_until_the_revert_finishes() {
        // Review Focus 5.
        let applied = Acknowledgements { kms_config_applied: true, ..Default::default() };
        for phase in [AwaitingKmsConfig, Rewriting, AwaitingPlaintextRemoval, Encrypted, RevertingKms] {
            assert_eq!(cleanup(phase, applied, true), CleanupStep::AwaitRevertAck, "{phase:?}");
        }
    }

    #[test]
    fn once_reverted_the_secrets_are_rewritten_then_kms_removal_is_awaited() {
        let reverted = Acknowledgements { kms_config_applied: true, kms_reverted: true, ..Default::default() };

        assert_eq!(cleanup(RevertingKms, reverted, true), CleanupStep::Decrypt);
        assert_eq!(cleanup(Decrypting, reverted, true), CleanupStep::Decrypt);
        assert_eq!(cleanup(AwaitingKmsRemoval, reverted, true), CleanupStep::AwaitKmsRemoval);
    }

    #[test]
    fn the_plugin_is_removed_only_after_kmsremoved_and_no_kms_provider_is_active() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };

        assert_eq!(cleanup(AwaitingKmsRemoval, removed, true), CleanupStep::AwaitKmsRemoval);
        assert_eq!(cleanup(AwaitingKmsRemoval, removed, false), CleanupStep::RemovePlugin);
    }

    #[test]
    fn kmsremoved_does_not_skip_the_decrypt_step() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };

        assert_eq!(cleanup(Decrypting, removed, false), CleanupStep::Decrypt);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib encryption_phase 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find function next_phase`.

- [ ] **Step 3: Implement**

Prepend to `src/encryption_phase.rs`:

```rust
use crate::etcd_encryption::{Acknowledgements, EncryptionPhase};

/// Everything the forward state machine needs, gathered by the reconciler.
/// A probe that errored is passed as `false`: failure never advances a phase.
#[derive(Debug, Clone, Copy)]
pub struct PhaseInputs {
    pub current: EncryptionPhase,
    pub acks: Acknowledgements,
    /// A Ready plugin pod on every control-plane node.
    pub plugin_ready: bool,
    /// The apiserver reports an active KMS provider.
    pub kms_active: bool,
    /// A canary Secret written and read back round-trips.
    pub canary_ok: bool,
    /// Every Secret was rewritten with zero failures.
    pub rewrite_complete: bool,
}

/// One forward step. Never regresses: a probe going negative after a phase was
/// reached leaves the phase alone (the reconciler reports `Degraded`
/// separately), and an acknowledgement is never reset.
pub fn next_phase(inputs: &PhaseInputs) -> EncryptionPhase {
    use EncryptionPhase::*;
    match inputs.current {
        Pending => InstallingPlugin,
        InstallingPlugin if inputs.plugin_ready => AwaitingKmsConfig,
        AwaitingKmsConfig if inputs.kms_active && inputs.acks.kms_config_applied => Rewriting,
        Rewriting if inputs.rewrite_complete => AwaitingPlaintextRemoval,
        AwaitingPlaintextRemoval
            if inputs.acks.plaintext_removed && inputs.kms_active && inputs.canary_ok =>
        {
            Encrypted
        }
        other => other,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CleanupInputs {
    pub phase: EncryptionPhase,
    pub acks: Acknowledgements,
    pub kms_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStep {
    /// Nothing depends on the plugin (or it is no longer in use): delete it.
    RemovePlugin,
    /// Publish the revert patch (identity first, kms second); wait for `kmsReverted`.
    AwaitRevertAck,
    /// Rewrite every Secret so none is left that only the plugin can read.
    Decrypt,
    /// Publish the remove-kms patch; wait for `kmsRemoved` and no KMS provider.
    AwaitKmsRemoval,
}

/// Which deletion step to run. The finalizer must keep returning an error for
/// every step except `RemovePlugin`: removing the plugin while the apiserver
/// still depends on it leaves the apiserver unable to read Secrets.
pub fn cleanup_step(inputs: &CleanupInputs) -> CleanupStep {
    use EncryptionPhase::*;
    let engaged = !matches!(inputs.phase, Pending | InstallingPlugin)
        && !(inputs.phase == AwaitingKmsConfig && !inputs.acks.kms_config_applied);
    if !engaged {
        return CleanupStep::RemovePlugin;
    }
    if !inputs.acks.kms_reverted {
        return CleanupStep::AwaitRevertAck;
    }
    if inputs.phase != AwaitingKmsRemoval {
        return CleanupStep::Decrypt;
    }
    if inputs.acks.kms_removed && !inputs.kms_active {
        return CleanupStep::RemovePlugin;
    }
    CleanupStep::AwaitKmsRemoval
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib encryption_phase`
Expected: PASS (12 tests).

- [ ] **Step 5: Commit**

```bash
git add src/encryption_phase.rs src/lib.rs
git commit -m "feat: EtcdEncryption phase and cleanup state machines

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Secret rewrite loop

**Files:**
- Create: `src/secret_rewrite.rs`
- Modify: `src/lib.rs` (`pub mod secret_rewrite;`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct SecretKey { pub namespace: String, pub name: String }` (`Clone, Debug, PartialEq, Eq`)
  - `pub struct SecretPage { pub keys: Vec<SecretKey>, pub next: Option<String> }`
  - `pub enum TouchOutcome { Rewritten, Gone, Conflict }`
  - `pub struct StoreError(pub String)` (`thiserror`, displays the string)
  - `pub trait SecretStore { async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError>; async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError>; }`
  - `pub const CONFLICT_RETRIES: usize = 3;`
  - `pub struct PageOutcome { pub seen: u64, pub rewritten: u64, pub failed: Vec<SecretKey>, pub next: Option<String> }`
  - `pub async fn rewrite_page<S: SecretStore>(store: &S, token: Option<&str>) -> Result<PageOutcome, StoreError>`
  - `pub struct KubeSecretStore { /* Api<Secret> */ }` with `pub fn new(client: kube::Client) -> Self`, implementing `SecretStore`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    fn key(name: &str) -> SecretKey {
        SecretKey { namespace: "ns".to_string(), name: name.to_string() }
    }

    #[derive(Default)]
    struct FakeStore {
        keys: Vec<SecretKey>,
        page_size: usize,
        /// name -> remaining conflicts to return before succeeding
        conflicts: Mutex<HashMap<String, usize>>,
        gone: HashSet<String>,
        denied: HashSet<String>,
        touched: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn with(names: &[&str], page_size: usize) -> Self {
            FakeStore { keys: names.iter().map(|n| key(n)).collect(), page_size, ..Default::default() }
        }
    }

    impl SecretStore for FakeStore {
        async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError> {
            let start: usize = token.map(|t| t.parse().unwrap()).unwrap_or(0);
            let end = (start + self.page_size).min(self.keys.len());
            Ok(SecretPage {
                keys: self.keys[start..end].to_vec(),
                next: (end < self.keys.len()).then(|| end.to_string()),
            })
        }

        async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError> {
            if self.denied.contains(&key.name) {
                return Err(StoreError("admission webhook denied the request".to_string()));
            }
            if self.gone.contains(&key.name) {
                return Ok(TouchOutcome::Gone);
            }
            let mut conflicts = self.conflicts.lock().unwrap();
            if let Some(remaining) = conflicts.get_mut(&key.name)
                && *remaining > 0
            {
                *remaining -= 1;
                return Ok(TouchOutcome::Conflict);
            }
            self.touched.lock().unwrap().push(key.name.clone());
            Ok(TouchOutcome::Rewritten)
        }
    }

    #[tokio::test]
    async fn rewrites_every_secret_on_a_page_and_reports_the_next_token() {
        let store = FakeStore::with(&["a", "b", "c"], 2);

        let first = rewrite_page(&store, None).await.unwrap();

        assert_eq!((first.seen, first.rewritten), (2, 2));
        assert!(first.failed.is_empty());
        assert_eq!(first.next.as_deref(), Some("2"));

        let second = rewrite_page(&store, first.next.as_deref()).await.unwrap();

        assert_eq!((second.seen, second.rewritten), (1, 1));
        assert_eq!(second.next, None);
        assert_eq!(*store.touched.lock().unwrap(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn a_conflict_is_retried_and_then_succeeds() {
        // Review Focus 1.
        let store = FakeStore::with(&["a"], 10);
        store.conflicts.lock().unwrap().insert("a".to_string(), 2);

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.rewritten, 1);
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn a_conflict_that_never_clears_is_counted_failed_after_bounded_retries() {
        let store = FakeStore::with(&["a"], 10);
        store.conflicts.lock().unwrap().insert("a".to_string(), 1000);

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.rewritten, 0);
        assert_eq!(outcome.failed, vec![key("a")]);
    }

    #[tokio::test]
    async fn a_secret_deleted_mid_run_is_done_not_failed() {
        // Review Focus 1.
        let mut store = FakeStore::with(&["a", "b"], 10);
        store.gone.insert("a".to_string());

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.rewritten, 2);
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn a_webhook_denial_fails_that_secret_but_the_rest_are_still_rewritten() {
        // Review Focus 2.
        let mut store = FakeStore::with(&["a", "b", "c"], 10);
        store.denied.insert("b".to_string());

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!(outcome.seen, 3);
        assert_eq!(outcome.rewritten, 2);
        assert_eq!(outcome.failed, vec![key("b")]);
        assert_eq!(*store.touched.lock().unwrap(), ["a", "c"]);
    }

    #[tokio::test]
    async fn an_empty_cluster_is_a_complete_empty_page() {
        let store = FakeStore::with(&[], 10);

        let outcome = rewrite_page(&store, None).await.unwrap();

        assert_eq!((outcome.seen, outcome.rewritten, outcome.next), (0, 0, None));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib secret_rewrite 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find trait SecretStore`.

- [ ] **Step 3: Implement**

Prepend to `src/secret_rewrite.rs`:

```rust
use k8s_openapi::api::core::v1::Secret;
use kube::api::{ListParams, PostParams};
use kube::{Api, Client};

/// A Secret's identity. Deliberately carries no data: the rewrite must never
/// hold, log or report a Secret's contents beyond the moment of the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretKey {
    pub namespace: String,
    pub name: String,
}

pub struct SecretPage {
    pub keys: Vec<SecretKey>,
    pub next: Option<String>,
}

pub enum TouchOutcome {
    Rewritten,
    /// Deleted since it was listed: nothing left to encrypt.
    Gone,
    /// Changed since it was read (HTTP 409): safe to retry.
    Conflict,
}

#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct StoreError(pub String);

/// The cluster's Secrets, as the rewrite loop sees them. A trait so the loop
/// is unit-testable without a cluster; `KubeSecretStore` is the real one.
#[allow(async_fn_in_trait)]
pub trait SecretStore {
    async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError>;
    /// Re-save the Secret unchanged so the apiserver stores it through the
    /// current encryption provider.
    async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError>;
}

/// Attempts per Secret before a persistent 409 is counted as a failure.
pub const CONFLICT_RETRIES: usize = 3;

pub struct PageOutcome {
    pub seen: u64,
    pub rewritten: u64,
    pub failed: Vec<SecretKey>,
    pub next: Option<String>,
}

/// Rewrites one page. A listing error aborts (the whole step is retried);
/// a per-Secret error is recorded and the page continues. Idempotent, so a
/// controller restart simply starts over.
pub async fn rewrite_page<S: SecretStore>(
    store: &S,
    token: Option<&str>,
) -> Result<PageOutcome, StoreError> {
    let page = store.list_page(token).await?;
    let mut outcome = PageOutcome {
        seen: page.keys.len() as u64,
        rewritten: 0,
        failed: Vec::new(),
        next: page.next,
    };
    for key in page.keys {
        let mut attempts = 0;
        loop {
            match store.touch(&key).await {
                Ok(TouchOutcome::Rewritten) | Ok(TouchOutcome::Gone) => {
                    outcome.rewritten += 1;
                    break;
                }
                Ok(TouchOutcome::Conflict) => {
                    attempts += 1;
                    if attempts >= CONFLICT_RETRIES {
                        outcome.failed.push(key.clone());
                        break;
                    }
                }
                Err(_) => {
                    outcome.failed.push(key.clone());
                    break;
                }
            }
        }
    }
    Ok(outcome)
}

const PAGE_SIZE: u32 = 100;

pub struct KubeSecretStore {
    api: Api<Secret>,
}

impl KubeSecretStore {
    pub fn new(client: Client) -> Self {
        KubeSecretStore { api: Api::all(client) }
    }
}

impl SecretStore for KubeSecretStore {
    async fn list_page(&self, token: Option<&str>) -> Result<SecretPage, StoreError> {
        let mut params = ListParams::default().limit(PAGE_SIZE);
        if let Some(token) = token {
            params = params.continue_token(token);
        }
        let list = self.api.list(&params).await.map_err(|err| StoreError(err.to_string()))?;
        let keys = list
            .items
            .iter()
            .map(|secret| SecretKey {
                namespace: secret.metadata.namespace.clone().unwrap_or_default(),
                name: secret.metadata.name.clone().unwrap_or_default(),
            })
            .collect();
        let next = list.metadata.continue_.filter(|token| !token.is_empty());
        Ok(SecretPage { keys, next })
    }

    async fn touch(&self, key: &SecretKey) -> Result<TouchOutcome, StoreError> {
        // Namespaced api for the write; `self.api` is cluster-wide.
        let namespaced: Api<Secret> = Api::namespaced(self.api.clone().into_client(), &key.namespace);
        let secret = match namespaced.get(&key.name).await {
            Ok(secret) => secret,
            Err(kube::Error::Api(status)) if status.code == 404 => return Ok(TouchOutcome::Gone),
            Err(err) => return Err(StoreError(err.to_string())),
        };
        // `replace` carries the object's resourceVersion, so a concurrent
        // change is a 409 rather than a lost update. The apiserver rewrites a
        // stored object whose on-disk form is stale for the current provider,
        // which is exactly what this is for.
        match namespaced.replace(&key.name, &PostParams::default(), &secret).await {
            Ok(_) => Ok(TouchOutcome::Rewritten),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(TouchOutcome::Gone),
            Err(kube::Error::Api(status)) if status.code == 409 => Ok(TouchOutcome::Conflict),
            Err(err) => Err(StoreError(err.to_string())),
        }
    }
}
```

> If `Api::into_client` is not available in kube 4.2, store the `Client` in `KubeSecretStore` instead (`client: Client`) and build `Api::namespaced(self.client.clone(), &key.namespace)`; keep `Api::all(self.client.clone())` for listing. Adjust the struct and `new` to match — the trait surface is unchanged.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib secret_rewrite`
Expected: PASS (6 tests). Then `cargo clippy --all-targets 2>&1 | tail -5` — expected no new warnings.

- [ ] **Step 5: Commit**

```bash
git add src/secret_rewrite.rs src/lib.rs
git commit -m "feat: idempotent Secret rewrite loop behind a SecretStore trait

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Apiserver probes

**Files:**
- Create: `src/encryption_probe.rs`
- Modify: `src/lib.rs` (`pub mod encryption_probe;`), `Cargo.toml` (add `http = "1"` under `[dependencies]`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub const KMS_METRIC_PREFIXES: &[&str] = &["apiserver_envelope_encryption_"];` (open verification item 1: confirm live)
  - `pub fn metrics_report_kms(metrics: &str) -> bool`
  - `pub const CANARY_NAME: &str = "etcd-encryption-canary"; pub const CANARY_NAMESPACE: &str = "kube-system";`
  - `pub fn canary_secret(value: &str) -> Secret`
  - `#[derive(thiserror::Error, Debug)] pub enum ProbeError { #[error("{0}")] Request(String), #[error("probe timed out")] Timeout }`
  - `pub async fn kms_active(client: &Client) -> Result<bool, ProbeError>`
  - `pub async fn canary_round_trips(client: &Client) -> Result<bool, ProbeError>`
  - `pub async fn delete_canary(client: &Client)` (best effort, ignores NotFound)

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib encryption_probe 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find function metrics_report_kms`.

- [ ] **Step 3: Implement**

Prepend to `src/encryption_probe.rs`:

```rust
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
    let write = api.patch(
        CANARY_NAME,
        &PatchParams::apply("platform-controller").force(),
        &Patch::Apply(&canary_secret(&value)),
    );
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib encryption_probe && cargo clippy --all-targets 2>&1 | tail -5`
Expected: PASS (4 tests); no new warnings. (If `kube::Client::request_text` takes a differently typed request in 4.2, adapt the `http::Request` body type; the pure functions and their tests do not change.)

- [ ] **Step 5: Commit**

```bash
git add src/encryption_probe.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: apiserver KMS probes (metrics and canary Secret)

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 7: The reconciler

**Files:**
- Create: `src/etcd_encryption_reconciler.rs`
- Modify: `src/lib.rs` (`pub mod etcd_encryption_reconciler;`)

**Interfaces:**
- Consumes: everything from Tasks 1–6, plus existing `crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME}`, `crate::apply::{apply_object, delete_object, resource_ref}`, `crate::ledger::{checkpoint_ledger, failure_ledger, ReconcileProgress}`.
- Produces (used by Task 8): `pub async fn reconcile_with_finalizer(obj: Arc<EtcdEncryption>, ctx: Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<EtcdEncryptionReconcileError>>` and `pub fn error_policy(obj: Arc<EtcdEncryption>, err: &kube::runtime::finalizer::Error<EtcdEncryptionReconcileError>, ctx: Arc<Context>) -> Action`.
- Also produces pure, tested helpers: `validate`, `ValidationError::reason`, `EtcdEncryptionReconcileError::failure_reason`, `daemonset_covers_all_control_plane`, `condition`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::{BarbicanSpec, KmsProviderKind, SecretNameRef};

    fn spec_with(platform_kind: PlatformKind) -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind,
            provider: KmsProviderKind::Barbican,
            barbican: Some(BarbicanSpec {
                image: "img:1".to_string(),
                cloud_config_secret_ref: SecretNameRef { name: "cc".to_string() },
            }),
            acknowledgements: Default::default(),
        }
    }

    #[test]
    fn accepts_talos_linux_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let err = validate("second", &spec_with(PlatformKind::TalosLinux)).unwrap_err();

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.acknowledgements.plaintext_removed = true;

        assert_eq!(validate("default", &spec).unwrap_err().reason(), "InvalidAcknowledgements");
    }

    #[test]
    fn plugin_is_ready_only_when_it_covers_every_control_plane_node() {
        // Review Focus 3.
        assert!(daemonset_covers_all_control_plane(3, 3, 3));
        assert!(!daemonset_covers_all_control_plane(2, 2, 3), "scheduled on fewer nodes than exist");
        assert!(!daemonset_covers_all_control_plane(3, 2, 3), "a pod is not Ready yet");
        assert!(!daemonset_covers_all_control_plane(0, 0, 0), "no control-plane nodes visible");
        assert!(!daemonset_covers_all_control_plane(0, 0, 3), "nothing scheduled yet");
    }

    #[test]
    fn failure_reasons_cover_every_reportable_variant_and_skip_the_rest() {
        let apply = EtcdEncryptionReconcileError::Apply(crate::apply::ApplyError::KindNotAvailable {
            api_version: "apps/v1".to_string(),
            kind: "DaemonSet".to_string(),
            timeout: std::time::Duration::from_secs(1),
            detail: "x".to_string(),
        });
        assert_eq!(apply.failure_reason(), Some("ApplyFailed"));
        assert_eq!(
            EtcdEncryptionReconcileError::Store(crate::secret_rewrite::StoreError("x".to_string())).failure_reason(),
            Some("RewriteFailed")
        );
        assert_eq!(EtcdEncryptionReconcileError::NotLeader.failure_reason(), None);
        assert_eq!(
            EtcdEncryptionReconcileError::CleanupBlocked("x".to_string()).failure_reason(),
            None,
            "a blocked cleanup is a wait, not a failure to report"
        );
    }

    #[test]
    fn condition_is_ready_true_only_when_requested() {
        let ok = condition("Ready", true, "Encrypted", "done", Some(3));
        let not_ok = condition("Ready", false, "Rewriting", "in progress", Some(3));

        assert_eq!((ok.status.as_str(), ok.observed_generation), ("True", Some(3)));
        assert_eq!(not_ok.status, "False");
    }

    #[test]
    fn patches_for_a_phase_include_everything_published_so_far() {
        let plan = crate::kms_provider::kms_plan(&spec_with(PlatformKind::TalosLinux)).unwrap();

        let early = patches_for(EncryptionPhase::AwaitingKmsConfig, &plan, &TalosPatches::default());
        assert!(early.enable_kms.is_some() && early.remove_identity.is_none());

        let later = patches_for(EncryptionPhase::AwaitingPlaintextRemoval, &plan, &early);
        assert!(later.enable_kms.is_some() && later.remove_identity.is_some());

        let before = patches_for(EncryptionPhase::InstallingPlugin, &plan, &TalosPatches::default());
        assert!(before.enable_kms.is_none(), "no patch before the plugin is ready everywhere");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib etcd_encryption_reconciler 2>&1 | tail -8` (after adding the `pub mod` line)
Expected: FAIL to compile — `cannot find function validate`.

- [ ] **Step 3: Implement**

Prepend to `src/etcd_encryption_reconciler.rs`. The file is long; it follows `snapshot_controller_reconciler.rs` for everything not spelled out here (leader gate, finalizer, error policy).

```rust
use crate::crd::{AppliedResourceRef, Condition, PlatformKind};
use crate::encryption_phase::{cleanup_step, next_phase, CleanupInputs, CleanupStep, PhaseInputs};
use crate::etcd_encryption::{
    EncryptionPhase, EtcdEncryption, EtcdEncryptionSpec, EtcdEncryptionSpecError, EtcdEncryptionStatus,
    RewriteProgress, TalosPatches,
};
use crate::kms_provider::{kms_plan, KmsPlan, PLUGIN_NAMESPACE};
use crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME};
use crate::secret_rewrite::{rewrite_page, KubeSecretStore, SecretStore, StoreError};
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::Node;
use kube::api::ListParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

const FIELD_MANAGER: &str = "platform-controller";
const WAITING_REQUEUE: Duration = Duration::from_secs(15);
const STEADY_REQUEUE: Duration = Duration::from_secs(300);

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "EtcdEncryption {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] EtcdEncryptionSpecError),
}

impl ValidationError {
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            ValidationError::UnsupportedPlatform(_) | ValidationError::UnsupportedName(_) => "Unsupported",
        }
    }
}

pub fn validate(name: &str, spec: &EtcdEncryptionSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::etcd_encryption::validate_etcd_encryption(spec)?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum EtcdEncryptionReconcileError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Apply(#[from] crate::apply::ApplyError),
    #[error("kubernetes API call failed: {0}")]
    Api(#[source] kube::Error),
    #[error("secret rewrite failed: {0}")]
    Store(#[from] StoreError),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
    #[error("not the leader; standing down")]
    NotLeader,
    /// Deletion is deliberately waiting on the operator or a probe. Always an
    /// `Err`: `kube::runtime::finalizer` strips the finalizer on any `Ok` from
    /// the Cleanup arm, and the plugin must outlive the apiserver's use of it.
    #[error("deletion is waiting: {0}")]
    CleanupBlocked(String),
}

impl EtcdEncryptionReconcileError {
    /// The `Ready=False` reason to report, or `None` when this error must not
    /// overwrite status. Exhaustive on purpose: a new variant forces a decision.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            EtcdEncryptionReconcileError::Apply(_) => Some("ApplyFailed"),
            EtcdEncryptionReconcileError::Api(_) => Some("ApiCallFailed"),
            EtcdEncryptionReconcileError::Store(_) => Some("RewriteFailed"),
            EtcdEncryptionReconcileError::Validation(_)
            | EtcdEncryptionReconcileError::Status(_)
            | EtcdEncryptionReconcileError::NotLeader
            | EtcdEncryptionReconcileError::CleanupBlocked(_) => None,
        }
    }
}

pub fn condition(type_: &str, ok: bool, reason: &str, message: &str, generation: Option<i64>) -> Condition {
    Condition {
        type_: type_.to_string(),
        status: if ok { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    }
}

/// A Ready plugin pod must exist on **every** control-plane node, and there
/// must be at least one: patch 1 triggers a rolling apiserver restart that
/// needs the socket on each node it lands on.
pub fn daemonset_covers_all_control_plane(desired: i32, ready: i32, control_plane_nodes: usize) -> bool {
    control_plane_nodes > 0 && desired >= 0 && desired as usize == control_plane_nodes && ready == desired
}

/// The patches visible once `phase` is reached: additive, never retracted.
pub fn patches_for(phase: EncryptionPhase, plan: &KmsPlan, current: &TalosPatches) -> TalosPatches {
    use EncryptionPhase::*;
    let mut patches = current.clone();
    if matches!(phase, AwaitingKmsConfig | Rewriting | AwaitingPlaintextRemoval | Encrypted) {
        patches.enable_kms = Some(crate::talos_patches::enable_kms(plan));
    }
    if matches!(phase, AwaitingPlaintextRemoval | Encrypted) {
        patches.remove_identity = Some(crate::talos_patches::remove_identity(plan));
    }
    patches
}

async fn write_status(
    api: &Api<EtcdEncryption>,
    name: &str,
    status: &EtcdEncryptionStatus,
) -> Result<(), EtcdEncryptionReconcileError> {
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(EtcdEncryptionReconcileError::Status)?;
    Ok(())
}

async fn plugin_ready(client: &kube::Client, plan: &KmsPlan) -> Result<bool, EtcdEncryptionReconcileError> {
    let daemonsets: Api<DaemonSet> = Api::namespaced(client.clone(), PLUGIN_NAMESPACE);
    let status = daemonsets
        .get_opt(&plan.daemonset_name)
        .await
        .map_err(EtcdEncryptionReconcileError::Api)?
        .and_then(|daemonset| daemonset.status);
    let Some(status) = status else { return Ok(false) };
    let nodes: Api<Node> = Api::all(client.clone());
    let control_plane = nodes
        .list(&ListParams::default().labels("node-role.kubernetes.io/control-plane"))
        .await
        .map_err(EtcdEncryptionReconcileError::Api)?
        .items
        .len();
    Ok(daemonset_covers_all_control_plane(status.desired_number_scheduled, status.number_ready, control_plane))
}

/// A probe error is "not yet": it is logged and never advances a phase.
async fn probe_kms_active(client: &kube::Client) -> bool {
    crate::encryption_probe::kms_active(client).await.unwrap_or_else(|err| {
        tracing::warn!(error = %err, "KMS metrics probe failed; treating as not active");
        false
    })
}

/// Rewrites every Secret, writing progress to status after each page.
/// Returns true only when every Secret was rewritten with zero failures.
async fn run_rewrite(
    api: &Api<EtcdEncryption>,
    name: &str,
    store: &impl SecretStore,
    status: &mut EtcdEncryptionStatus,
) -> Result<bool, EtcdEncryptionReconcileError> {
    let mut progress = RewriteProgress::default();
    let mut token: Option<String> = None;
    loop {
        let page = rewrite_page(store, token.as_deref()).await?;
        progress.total += page.seen;
        progress.rewritten += page.rewritten;
        progress.failed += page.failed.len() as u64;
        for key in &page.failed {
            // Identity only, never data.
            tracing::warn!(namespace = %key.namespace, secret = %key.name, "failed to rewrite secret");
        }
        status.rewrite = progress.clone();
        write_status(api, name, status).await?;
        match page.next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    Ok(progress.failed == 0)
}

pub async fn reconcile(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    let mut progress = crate::ledger::ReconcileProgress::default();
    let result = reconcile_inner(obj.clone(), ctx.clone(), &mut progress).await;
    if let Err(err) = &result
        && let Some(reason) = err.failure_reason()
    {
        record_failure(&obj, &ctx, &progress, reason, &err.to_string()).await;
    }
    result
}

/// Best effort. Keeps the phase and every other field: a failure is a
/// condition, never a regression of the protocol position.
async fn record_failure(
    obj: &EtcdEncryption,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    let mut status = obj.status.clone().unwrap_or_default();
    status.applied_resources = crate::ledger::failure_ledger(&status.applied_resources, progress.desired.as_deref());
    status.conditions = vec![condition("Ready", false, reason, message, obj.metadata.generation)];
    if let Err(err) = write_status(&api, &name, &status).await {
        tracing::warn!(installation = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, EtcdEncryptionReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }
    let name = obj.name_any();
    let generation = obj.metadata.generation;
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    let mut status = obj.status.clone().unwrap_or_default();
    status.observed_generation = generation.unwrap_or(0);

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(installation = %name, error = %err, "validation failed");
        status.conditions = vec![condition("Ready", false, err.reason(), &err.to_string(), generation)];
        write_status(&api, &name, &status).await?;
        return Err(EtcdEncryptionReconcileError::Validation(err));
    }
    let plan = kms_plan(&obj.spec).map_err(ValidationError::Spec)?;

    // Checkpoint the ledger before the first apply, like every other component.
    let desired = vec![crate::apply::resource_ref(&plan.daemonset)];
    progress.desired = Some(desired.clone());
    if let Some(ledger) = crate::ledger::checkpoint_ledger(&status.applied_resources, &desired) {
        status.applied_resources = ledger;
        status.conditions = vec![condition("Ready", false, "Applying", "applying the KMS plugin", generation)];
        write_status(&api, &name, &status).await?;
    }
    crate::apply::apply_object(&ctx.client, &plan.daemonset, FIELD_MANAGER).await?;

    // Gather the facts. Probes only run once they can matter.
    let mut inputs = PhaseInputs {
        current: status.phase,
        acks: obj.spec.acknowledgements,
        plugin_ready: plugin_ready(&ctx.client, &plan).await?,
        kms_active: false,
        canary_ok: false,
        rewrite_complete: false,
    };
    if !matches!(inputs.current, EncryptionPhase::Pending | EncryptionPhase::InstallingPlugin) {
        inputs.kms_active = probe_kms_active(&ctx.client).await;
    }
    if matches!(inputs.current, EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted) {
        inputs.canary_ok = crate::encryption_probe::canary_round_trips(&ctx.client)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "canary probe failed; treating as not round-tripping");
                false
            });
    }

    // Walk forward as far as the facts allow in one reconcile.
    let store = KubeSecretStore::new(ctx.client.clone());
    loop {
        if inputs.current == EncryptionPhase::Rewriting {
            inputs.rewrite_complete = run_rewrite(&api, &name, &store, &mut status).await?;
        }
        let next = next_phase(&inputs);
        if next == inputs.current {
            break;
        }
        tracing::info!(installation = %name, from = ?inputs.current, to = ?next, "phase transition");
        inputs.current = next;
        if matches!(next, EncryptionPhase::Rewriting | EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted)
            && !inputs.kms_active
        {
            inputs.kms_active = probe_kms_active(&ctx.client).await;
        }
        if matches!(next, EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted) && !inputs.canary_ok {
            inputs.canary_ok = crate::encryption_probe::canary_round_trips(&ctx.client).await.unwrap_or(false);
        }
    }

    status.phase = inputs.current;
    status.talos_patches = patches_for(status.phase, &plan, &status.talos_patches);
    let (ready, degraded) = match status.phase {
        EncryptionPhase::Encrypted => (true, !(inputs.kms_active && inputs.canary_ok && inputs.plugin_ready)),
        _ => (false, false),
    };
    status.conditions = vec![condition("Ready", ready && !degraded, &format!("{:?}", status.phase), &phase_message(status.phase), generation)];
    if degraded {
        status.conditions.push(condition(
            "Degraded",
            true,
            "ProbeNegative",
            "a probe that passed earlier is now failing; the phase is not regressed. Check the plugin pods and the apiserver",
            generation,
        ));
    }
    write_status(&api, &name, &status).await?;

    Ok(Action::requeue(if status.phase == EncryptionPhase::Encrypted { STEADY_REQUEUE } else { WAITING_REQUEUE }))
}

fn phase_message(phase: EncryptionPhase) -> String {
    use EncryptionPhase::*;
    match phase {
        Pending | InstallingPlugin => "installing the KMS plugin on every control-plane node",
        AwaitingKmsConfig => "apply status.talosPatches.enableKms with talosctl, then set spec.acknowledgements.kmsConfigApplied",
        Rewriting => "re-encrypting every Secret through KMS",
        AwaitingPlaintextRemoval => "apply status.talosPatches.removeIdentity with talosctl, then set spec.acknowledgements.plaintextRemoved",
        Encrypted => "Secrets are encrypted at rest through KMS; verified by apiserver probes",
        RevertingKms | Decrypting | AwaitingKmsRemoval => "reverting encryption before deletion",
    }
    .to_string()
}

pub fn error_policy(
    _obj: Arc<EtcdEncryption>,
    _err: &kube::runtime::finalizer::Error<EtcdEncryptionReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

pub async fn cleanup(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch; see `reconciler::cleanup`.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(EtcdEncryptionReconcileError::NotLeader);
    }
    let name = obj.name_any();
    let generation = obj.metadata.generation;
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    let mut status = obj.status.clone().unwrap_or_default();
    let plan = kms_plan(&obj.spec).map_err(ValidationError::Spec)?;
    let kms_active = probe_kms_active(&ctx.client).await;

    let step = cleanup_step(&CleanupInputs { phase: status.phase, acks: obj.spec.acknowledgements, kms_active });
    tracing::info!(installation = %name, phase = ?status.phase, ?step, "cleanup step");

    match step {
        CleanupStep::RemovePlugin => {
            for reference in status.applied_resources.iter().rev() {
                tracing::info!(kind = %reference.kind, resource = %reference.name, "deleting applied resource");
                crate::apply::delete_object(&ctx.client, reference).await?;
            }
            crate::encryption_probe::delete_canary(&ctx.client).await;
            Ok(Action::await_change())
        }
        CleanupStep::AwaitRevertAck => {
            status.phase = EncryptionPhase::RevertingKms;
            status.talos_patches.revert = Some(crate::talos_patches::revert(&plan));
            status.conditions = vec![condition("Ready", false, "RevertingKms", "apply status.talosPatches.revert with talosctl, then set spec.acknowledgements.kmsReverted", generation)];
            write_status(&api, &name, &status).await?;
            Err(EtcdEncryptionReconcileError::CleanupBlocked("waiting for spec.acknowledgements.kmsReverted".to_string()))
        }
        CleanupStep::Decrypt => {
            status.phase = EncryptionPhase::Decrypting;
            write_status(&api, &name, &status).await?;
            let store = KubeSecretStore::new(ctx.client.clone());
            if run_rewrite(&api, &name, &store, &mut status).await? {
                status.phase = EncryptionPhase::AwaitingKmsRemoval;
                status.talos_patches.remove_kms = Some(crate::talos_patches::remove_kms());
                status.conditions = vec![condition("Ready", false, "AwaitingKmsRemoval", "apply status.talosPatches.removeKms with talosctl, then set spec.acknowledgements.kmsRemoved", generation)];
                write_status(&api, &name, &status).await?;
                Err(EtcdEncryptionReconcileError::CleanupBlocked("waiting for spec.acknowledgements.kmsRemoved".to_string()))
            } else {
                Err(EtcdEncryptionReconcileError::CleanupBlocked("some Secrets could not be rewritten; retrying".to_string()))
            }
        }
        CleanupStep::AwaitKmsRemoval => Err(EtcdEncryptionReconcileError::CleanupBlocked(
            "waiting for spec.acknowledgements.kmsRemoved and for the apiserver to stop reporting a KMS provider".to_string(),
        )),
    }
}

pub async fn reconcile_with_finalizer(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<EtcdEncryptionReconcileError>> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    kube::runtime::finalizer(&api, FINALIZER_NAME, obj, |event| async move {
        match event {
            kube::runtime::finalizer::Event::Apply(obj) => reconcile(obj, ctx).await,
            kube::runtime::finalizer::Event::Cleanup(obj) => cleanup(obj, ctx).await,
        }
    })
    .await
}
```

> The unused import `AppliedResourceRef` may be flagged by clippy; drop it if so. `crate::apply::ApplyError::KindNotAvailable` fields in the test are copied from `snapshot_controller_reconciler.rs`'s identical test — match them if they differ.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib etcd_encryption_reconciler && cargo clippy --all-targets 2>&1 | tail -10`
Expected: PASS (7 tests); no new clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/etcd_encryption_reconciler.rs src/lib.rs
git commit -m "feat: EtcdEncryption reconciler with the acknowledgement protocol

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Wire the controller into `main.rs`

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `etcd_encryption_reconciler::{reconcile_with_finalizer, error_policy}` (Task 7), `EtcdEncryption` (Task 1).

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/main.rs`, next to `deletion_requested_works_for_the_snapshot_controller_kind_too`:

```rust
    #[test]
    fn deletion_requested_works_for_the_etcd_encryption_kind_too() {
        use platform_controller::etcd_encryption::EtcdEncryption;
        let mut object: EtcdEncryption = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "EtcdEncryption",
            "metadata": { "name": "default" },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "barbican",
                "barbican": { "image": "img:1", "cloudConfigSecretRef": { "name": "cc" } }
            }
        }))
        .expect("EtcdEncryption should deserialize");

        assert_eq!(deletion_requested(&object), None);

        object.metadata.deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()));

        assert_eq!(deletion_requested(&object), Some(1));
    }
```

- [ ] **Step 2: Run to verify it fails or passes for the right reason**

Run: `cargo test --bin platform-controller deletion_requested 2>&1 | tail -8`
Expected: PASS already for `deletion_requested` itself (it is generic) — this test pins that the new kind goes through the same filter. If `chrono::Utc::now()` does not fit `Time` in k8s-openapi 0.28 (it wraps `jiff::Timestamp`), copy the construction used by the neighbouring snapshot-controller test instead.

- [ ] **Step 3: Wire the controller**

Add imports near the other component imports:

```rust
use platform_controller::etcd_encryption::EtcdEncryption;
use platform_controller::etcd_encryption_reconciler;
```

After the snapshot-controller `Controller` definition and before `let mut sigterm = ...`, add:

```rust
    // The etcd-encryption component gets its own watcher, store and
    // Controller too, with the same predicate filter and the same Context
    // (one leader lease).
    let etcd_encryption_api: Api<EtcdEncryption> = Api::all(client.clone());
    let (etcd_encryption_reader, etcd_encryption_writer) = reflector::store();
    let etcd_encryptions = watcher(etcd_encryption_api, watcher::Config::default())
        .default_backoff()
        .reflect(etcd_encryption_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );
```

The snapshot controller currently takes `context` by move (`context,`). Change that `.run(..., context,)` argument to `context.clone(),` and give the new controller the move:

```rust
    let etcd_encryption_controller = Controller::for_stream(etcd_encryptions, etcd_encryption_reader)
        .run(
            etcd_encryption_reconciler::reconcile_with_finalizer,
            etcd_encryption_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled etcd encryption"),
                Err(err) => tracing::error!(error = %err, "etcd encryption reconcile failed"),
            }
        });
```

Add to the `tokio::select!`: `_ = etcd_encryption_controller => {}`.

> **Predicate caveat (verify, do not assume):** `predicates::generation` ignores status-only writes, which is what the other six want. This controller, though, advances on **spec** changes (the acknowledgements — generation bumps, so they pass) and on **cluster state** (plugin readiness, probes), which it re-checks on its own 15 s requeue (`WAITING_REQUEUE`). So no extra trigger is needed; do not remove the generation filter.

- [ ] **Step 4: Build and test**

Run: `cargo build 2>&1 | tail -5 && cargo test --bin platform-controller`
Expected: builds; tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: run the EtcdEncryption controller under the shared leader lease

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Example manifest and its test

**Files:**
- Create: `examples/etcd-encryption.yaml`, `tests/etcd_encryption_example.rs`

**Interfaces:**
- Consumes: `EtcdEncryption`, `etcd_encryption_reconciler::validate`, `kms_provider::kms_plan`.

- [ ] **Step 1: Write the failing test**

`tests/etcd_encryption_example.rs`:

```rust
use platform_controller::etcd_encryption::EtcdEncryption;
use platform_controller::etcd_encryption_reconciler;
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/etcd-encryption.yaml";

fn load() -> EtcdEncryption {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as an EtcdEncryption")
}

#[test]
fn example_is_a_single_etcd_encryption_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "EtcdEncryption");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    etcd_encryption_reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_starts_with_every_acknowledgement_false() {
    let acks = load().spec.acknowledgements;

    assert!(!acks.kms_config_applied && !acks.plaintext_removed && !acks.kms_reverted && !acks.kms_removed);
}

#[test]
fn example_builds_a_plugin_plan() {
    let plan = platform_controller::kms_provider::kms_plan(&load().spec).expect("plan builds");

    assert_eq!(plan.daemonset_name, "barbican-kms");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test etcd_encryption_example 2>&1 | tail -6`
Expected: FAIL — `the example should exist`.

- [ ] **Step 3: Write the example**

`examples/etcd-encryption.yaml`:

```yaml
# Sample EtcdEncryption for a self-hosted Talos Linux cluster on OpenStack:
# installs the Barbican KMS plugin, re-encrypts every Secret through KMS, and
# walks the cluster to "no plaintext Secrets in etcd".
#
# The controller cannot change the apiserver's configuration (on Talos that
# lives in the machine config, applied through the Talos API, which this
# controller never touches). So this is a protocol: the controller publishes
# the exact Talos patches in .status.talosPatches, you apply them, and you
# acknowledge each step below. Full walk-through, including how to verify:
# docs/runbooks/etcd-encryption-verification.md
#
# BEFORE applying this:
#
# 1. Create a 256-bit AES key in Barbican and put its id in cloud.conf under
#    [KeyManager] key-id (see
#    https://github.com/kubernetes/cloud-provider-openstack/blob/master/docs/barbican-kms-plugin/using-barbican-kms-plugin.md).
#    The controller never creates, rotates or deletes keys. LOSING THIS KEY
#    MAKES EVERY SECRET IN THE CLUSTER UNREADABLE.
#
# 2. Create the credentials Secret in kube-system. The key MUST be `cloud.conf`:
#
#      kubectl -n kube-system create secret generic barbican-kms-cloud-config \
#        --from-file=cloud.conf=./cloud.conf
#
# Then:
#
#   kubectl apply -f examples/etcd-encryption.yaml
#   kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}'
#
# Leave every acknowledgement false until the status asks for it.
apiVersion: platform.rye.ninja/v1alpha1
kind: EtcdEncryption
metadata:
  name: default
spec:
  platformKind: talos-linux
  provider: barbican
  barbican:
    # The plugin image, pinned. Match the tag to your Kubernetes/OpenStack
    # release; this controller does not choose one for you.
    image: registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0
    cloudConfigSecretRef:
      name: barbican-kms-cloud-config
  acknowledgements:
    kmsConfigApplied: false   # true once talosPatches.enableKms is applied
    plaintextRemoved: false   # true once talosPatches.removeIdentity is applied
    kmsReverted: false        # deletion only: talosPatches.revert applied
    kmsRemoved: false         # deletion only: talosPatches.removeKms applied
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --test etcd_encryption_example`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add examples/etcd-encryption.yaml tests/etcd_encryption_example.rs
git commit -m "feat: EtcdEncryption example manifest and schema test

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 10: Live verification assets, docs and memory

**Files:**
- Create: `tests/integration_etcd_encryption.rs`, `docs/runbooks/etcd-encryption-verification.md`, `docs/memory/etcd-encryption-2026-09.md`
- Modify: `docs/memory/MEMORY.md`, `docs/memory/rbac-cluster-admin-tradeoff.md`, `deploy/README.md`

**Interfaces:**
- Consumes: `EtcdEncryption`, `EncryptionPhase`.

- [ ] **Step 1: Write the ignored integration test**

`tests/integration_etcd_encryption.rs`:

```rust
// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough for this narrow check; no OpenStack is
// needed for it to reach AwaitingKmsConfig only if the plugin pods can start,
// so on a cluster without Barbican use the runbook instead). With the
// controller running and all seven CRDs Established:
//
//   kubectl apply -f examples/etcd-encryption.yaml
//   cargo test --test integration_etcd_encryption -- --ignored --nocapture
//
// This checks only what the controller can do by itself: install the plugin
// DaemonSet and publish patch 1. It does NOT apply any Talos patch; the rest
// of the protocol is a manual runbook step
// (docs/runbooks/etcd-encryption-verification.md). It deletes the resource at
// the end, which, with no acknowledgement set, removes the plugin immediately.

use k8s_openapi::api::apps::v1::DaemonSet;
use kube::api::{Api, DeleteParams};
use kube::Client;
use platform_controller::etcd_encryption::{EncryptionPhase, EtcdEncryption};
use std::time::Duration;

async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what} did not happen within {timeout:?}");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
#[ignore = "needs a live cluster with the controller running; see the header comment"]
async fn installs_the_plugin_and_publishes_the_enable_kms_patch() {
    let client = Client::try_default().await.expect("a kubeconfig for the test cluster");
    let encryptions: Api<EtcdEncryption> = Api::all(client.clone());
    let daemonsets: Api<DaemonSet> = Api::namespaced(client.clone(), "kube-system");

    eventually("the barbican-kms DaemonSet to exist", Duration::from_secs(120), || async {
        daemonsets.get_opt("barbican-kms").await.unwrap().is_some()
    })
    .await;

    eventually("the resource to reach AwaitingKmsConfig", Duration::from_secs(300), || async {
        encryptions
            .get("default")
            .await
            .ok()
            .and_then(|e| e.status)
            .is_some_and(|s| s.phase == EncryptionPhase::AwaitingKmsConfig)
    })
    .await;

    let status = encryptions.get("default").await.unwrap().status.unwrap();
    let patch = status.talos_patches.enable_kms.expect("patch 1 is published");
    assert!(patch.contains("KubeEtcdEncryptionConfig"), "{patch}");

    encryptions.delete("default", &DeleteParams::default()).await.unwrap();
    eventually("the plugin to be removed on delete", Duration::from_secs(120), || async {
        daemonsets.get_opt("barbican-kms").await.unwrap().is_none()
    })
    .await;
}
```

- [ ] **Step 2: Confirm it compiles and is skipped by default**

Run: `cargo test --test integration_etcd_encryption`
Expected: `1 ignored`, no failures.

- [ ] **Step 3: Write the runbook**

`docs/runbooks/etcd-encryption-verification.md` must contain, in this order, each as a numbered section with the exact `kubectl`/`talosctl` commands and an "Expected:" line (follow the layout of `docs/runbooks/snapshot-controller-verification.md`):

0. **Prerequisites:** a Talos cluster with `CniInstallation` Ready; the Barbican key and `cloud.conf` Secret from `examples/etcd-encryption.yaml`; a baseline plaintext check: `kubectl create secret generic plain-before --from-literal=k=v`, then read it straight from etcd (`talosctl -n <cp> etcd ...` / `etcdctl get /registry/secrets/default/plain-before`) and confirm it is readable plaintext.
1. **Apply and reach `AwaitingKmsConfig`:** apply the CRD, bootstrap and example; `kubectl get etcdenc default -o jsonpath='{.status.phase}'`; `kubectl -n kube-system get ds barbican-kms` shows one Ready pod per control-plane node; `ls /var/lib/kms/kms.sock` on a node via `talosctl ls`.
2. **Patch 1:** `kubectl get etcdenc default -o jsonpath='{.status.talosPatches.enableKms}' > patch1.yaml`; `talosctl -n <cp1> patch machineconfig --patch @patch1.yaml` one control-plane node at a time, waiting for the apiserver to return between nodes; then `kubectl patch etcdenc default --type merge -p '{"spec":{"acknowledgements":{"kmsConfigApplied":true}}}'`. Expected: phase moves to `Rewriting` then `AwaitingPlaintextRemoval`; `status.rewrite.failed` is `0`.
3. **Verify encryption in etcd:** re-run the baseline read for `plain-before` — expected: now starts with `k8s:enc:kms:v2:barbican:` and is **not** readable.
4. **Patch 2:** as step 2 with `.status.talosPatches.removeIdentity` and `plaintextRemoved`. Expected: `Encrypted`, `Ready=True`.
5. **Prove plaintext is rejected:** with the plugin scaled to zero (`kubectl -n kube-system patch ds barbican-kms -p '{"spec":{"template":{"spec":{"nodeSelector":{"x":"y"}}}}}'`) reading any Secret must fail (apiserver cannot decrypt); restore it. This proves the identity fallback is gone. **Do not run this on a cluster you care about.**
6. **Delete protocol:** `kubectl delete etcdenc default`; it stays `Terminating`; walk `status.talosPatches.revert` → `kmsReverted`, then `removeKms` → `kmsRemoved`; expected: the DaemonSet disappears last and the baseline Secret is plaintext again. Also verify a delete **before** any acknowledgement removes the plugin immediately.
7. **Findings to record** (open verification items from the spec): (a) the exact `apiserver_envelope_encryption_*` metric names seen at `kubectl get --raw /metrics | grep envelope` and update `KMS_METRIC_PREFIXES` in `src/encryption_probe.rs`; (b) whether Talos needed `cluster.apiServer.extraVolumes` as generated or a different mounting mechanism, and the exact accepted shape of the `KubeEtcdEncryptionConfig` KMS block — correct `src/talos_patches.rs` and its tests if either differed; (c) the image tag that worked; (d) whether any admission webhook rejected a no-op Secret `update`.

- [ ] **Step 4: Write memory, ledger row and deploy docs**

`docs/memory/etcd-encryption-2026-09.md`:

```markdown
---
name: etcd-encryption-2026-09
description: EtcdEncryption slice, 2026-09-30 - seventh CRD; operator-acknowledged Talos-patch protocol because the controller never holds a Talos credential; Barbican first, other providers designed for; NOT live-verified yet
metadata:
  type: project
---

`EtcdEncryption` (cluster-scoped singleton `default`, shortname `etcdenc`) installs the Barbican KMS plugin, re-encrypts every Secret through KMS and walks the cluster to no plaintext Secrets in etcd. Spec: `docs/superpowers/specs/2026-09-30-etcd-encryption-design.md`; plan: `docs/superpowers/plans/2026-09-30-etcd-encryption.md`; acceptance: `docs/runbooks/etcd-encryption-verification.md`.

**Why a protocol, not just a reconcile:** on Talos the apiserver config lives in the machine config, applied through the Talos API, which this controller never touches. The controller publishes the exact patches in `status.talosPatches`, the operator applies them, and `spec.acknowledgements.*` are the gates. The controller also probes (apiserver `/metrics` + a canary Secret) and advances only when probe and acknowledgement agree.

**Non-obvious facts:**
- No `keyId` field: the Barbican plugin reads the key only from `cloud.conf`'s `[KeyManager] key-id`.
- Deleting is three operator steps, not one: revert patch (identity FIRST, kms second) -> rewrite all Secrets -> remove-kms patch -> only then delete the plugin. Identity-only straight away would make every KMS-encrypted Secret unreadable.
- No `Failed` phase: position in the protocol must survive failures, so failures are a `Ready=False` condition.
- The finalizer's Cleanup arm must never return `Ok` while the plugin is still needed (kube's finalizer strips the finalizer on any `Ok`).
- The plugin DaemonSet is built in Rust, not rendered from a chart; upstream ships only a raw `ds.yaml`.
- Unverified until the runbook is run: the `apiserver_envelope_encryption_*` metric names, the Talos `KubeEtcdEncryptionConfig` KMS block and socket-mount shape, the image tag.
- Not built: Azure, AWS, GCP, OCI providers (one builder + one enum variant each), a Talos-API opt-in, key management, encrypting non-Secret resources.

Related: [[rbac-cluster-admin-tradeoff]], [[wait-for-crd-established]], [[cloud-controller-manager-2026-09]].
```

Append to `docs/memory/MEMORY.md`:

```
- [EtcdEncryption slice](etcd-encryption-2026-09.md) — seventh CRD; operator-acknowledged Talos-patch protocol (controller holds no Talos credential); Barbican first; deletion is a 3-step revert; not live-verified yet
```

Add this row to the permission-ledger table in `docs/memory/rbac-cluster-admin-tradeoff.md` (after the last table row):

```
| `""` (core) | `secrets` — **`get`, `list`, `update` on every Secret in every namespace**; `create/patch/get/delete` on `kube-system/etcd-encryption-canary` | `EtcdEncryption` (added 2026-09-30, [[etcd-encryption-2026-09]]) | The Secret rewrite is the widest grant in the ledger: it must read and re-save all Secrets. Scoping down cannot narrow this by namespace. Also `get` on `nonResourceURLs: /metrics` and `apps` `daemonsets` + `nodes` `list`. |
```

In `deploy/README.md`, change "all four CRDs" if still present to "all of the CRDs", and add a short section at the end:

```markdown
## etcd Secret encryption (optional)

`examples/etcd-encryption.yaml` is an `EtcdEncryption` that encrypts every Secret at rest through an external KMS (OpenStack Barbican today). Unlike the other components it is a **protocol**: on Talos the apiserver's encryption config is part of the machine config, applied through the Talos API, which this controller never uses. The controller installs the KMS plugin, publishes each Talos patch in `.status.talosPatches`, re-encrypts every Secret, and verifies the result; you apply each patch with `talosctl` and acknowledge it in `spec.acknowledgements`. See `docs/runbooks/etcd-encryption-verification.md` for the full walk-through.

**You create and own the KMS key; losing it makes every Secret in the cluster unreadable.** Deleting the resource after the first patch is applied is also a multi-step process (it must re-save every Secret as plaintext before the plugin can go), so it stays `Terminating` until you acknowledge each step.
```

- [ ] **Step 5: Commit**

```bash
git add tests/integration_etcd_encryption.rs docs/runbooks/etcd-encryption-verification.md docs/memory/etcd-encryption-2026-09.md docs/memory/MEMORY.md docs/memory/rbac-cluster-admin-tradeoff.md deploy/README.md
git commit -m "docs: EtcdEncryption runbook, integration test, memory and ledger

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 11: Whole-branch verification

**Files:** none new.

- [ ] **Step 1: Run the full CI commands**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | tail -15 && cargo clippy --all-targets 2>&1 | tail -10`
Expected: build OK; every test passes (ignored integration tests stay ignored); clippy reports no new warnings.

- [ ] **Step 2: Confirm the CRD manifest is fresh**

Run: `cargo run -q --bin crdgen | diff - deploy/crd.yaml && echo fresh`
Expected: `fresh`.

- [ ] **Step 3: Spec/plan reconciliation check**

Run: `grep -n "keyId" docs/superpowers/specs/2026-09-30-etcd-encryption-design.md src examples deploy`
Expected: only the explanatory "There is no `keyId` field" sentence in the spec; no code or example reference.

- [ ] **Step 4: Report honestly**

State plainly in the PR description that this slice is **not live-verified**: the four open verification items in the spec (metric names, Talos KMS patch and mount shape, image tag, webhook behaviour on no-op updates) are settled only by running `docs/runbooks/etcd-encryption-verification.md` on a real cluster.
