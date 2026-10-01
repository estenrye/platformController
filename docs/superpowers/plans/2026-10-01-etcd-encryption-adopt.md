# EtcdEncryption Adopt Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the v0.1.11 fresh-install flow of `EtcdEncryption` with an observe / migrate / verify controller for clusters that already run a KMS plugin (static pods): it detects what the apiserver is configured to write with, rewrites every Secret through the target KMS provider on request, and proves per apiserver that nothing is left under a legacy provider.

**Architecture:** A stateless derivation: each reconcile verifies every control-plane apiserver directly (readiness, which provider it *writes* with via a canary write and the `to_storage` counter delta, which providers it *reads* with via a full limit-paged list and the `from_storage` delta), then derives the phase purely from that evidence plus the spec. Pure units (metrics parser, per-node verdict, cluster derivation) sit behind an `ApiserverProbe` trait with an in-memory fake, so every branch is tested without a cluster. The controller owns nothing in the cluster, so it has no finalizer, ledger, DaemonSet or cleanup.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, `serde_json`, `thiserror`, `tokio`, `http` 1 (already a dependency).

**Spec:** [docs/superpowers/specs/2026-10-01-etcd-encryption-adopt-design.md](../specs/2026-10-01-etcd-encryption-adopt-design.md). It supersedes `2026-09-30-etcd-encryption-design.md`. Executors read both only for history; the new spec is authoritative.

## Global Constraints

Every task's requirements implicitly include this section. Values are from the spec unless noted.

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `EtcdEncryption`, **cluster-scoped**, shortname `etcdenc`, plural `etcdencryptions`, singleton named `default`. This is a **breaking** change to the v0.1.11 schema.
- Spec fields: `platformKind` (only `talos-linux`), `kmsProviderName` (string, deserialized with default `""`, rejected at validation as `InvalidSpec` if empty, containing whitespace, or containing `:`), `rewrite` (`Disabled` default | `Enabled`), `acknowledgements.legacyProvidersRemoved` (bool, default false). **Removed:** `provider`, `barbican`, and the acknowledgements `kmsConfigApplied`, `plaintextRemoved`, `kmsReverted`, `kmsRemoved`.
- Target storage prefix: `k8s:enc:kms:v2:<kmsProviderName>:`. **Top-level** prefixes are those beginning `k8s:enc:`; the inner `key2:` sub-prefix seen live is ignored so reads are not double-counted. Reads with an **empty** prefix are legacy (identity/plaintext; not yet observed live). Everything top-level that is not the target prefix is legacy. There is no `legacyProviders` field.
- Phases (`status.phase`): `Observing`, `NotConfigured`, `Migrating`, `ReadyToRemoveLegacy`, `Verified`. The phase is **derived each reconcile**, not remembered: no persisted protocol position, no acknowledgement-after-publication mechanism, no ledger, no `patchGenerations`. There is no `Failed` phase: validation failure is a `Ready=False` condition with a reason.
- All status counters are `i64` (`u64` emits an `unrecognized format "uint64"` CRD warning).
- Per-node verification (each bounded by `tokio::time::timeout_at`): discover control-plane nodes by label `node-role.kubernetes.io/control-plane` and their `InternalIP`, port **6443**; per-node client from the in-cluster config with `cluster_url` overridden (IPv6 addresses bracketed) and `tls_server_name = "kubernetes.default.svc"`. (1) `GET /readyz?verbose` and record whether `kms-providers` is present and ok; (2) **writer check**: snapshot `apiserver_storage_transformation_operations_total{resource="secrets",status="OK"}`, write the canary Secret through this node, snapshot again; the writer is the target only if the `to_storage` delta of the target prefix is positive and no other top-level prefix rose; (3) **reader check**: snapshot, **list every Secret with a `limit`** (no `resourceVersion`, so it reads from etcd), snapshot; `from_storage` delta by prefix is the node's reads; the check is *complete* only if the deltas sum to at least the number of Secrets listed. A negative delta (counter reset by an apiserver restart) is "cannot verify".
- Derivation (spec, with one tightening noted below): (1) no nodes, or any node unverifiable → `Observing`; (2) no node's writer is the target: `NotConfigured` if no node reports `kms-providers`, else `Observing`; (3) writers differ across nodes → `Observing` (mixed), never `Migrating`; (4) every writer is the target: if every node's reader is complete with zero legacy reads → `Verified` when `legacyProvidersRemoved` is true **and** the canary round-trips, else `ReadyToRemoveLegacy`; otherwise → `Migrating` if `rewrite: Enabled` **and** at least one node's reader is complete with legacy reads > 0, else `Observing`. **Tightening of the spec's rule 4:** an *incomplete or unverifiable* reader check never triggers a rewrite (it would repeat full rewrites every 30 s while never being able to confirm); it is `Observing` with a "cannot verify reads" reason. Task 3 amends the spec accordingly.
- Rewrite runs only with `rewrite: Enabled`, only in the `Migrating` derivation, and reuses `secret_rewrite::rewrite_page` unchanged. It never logs Secret data.
- The controller adds **no finalizer** and has no cleanup. A CR created by v0.1.11 carries `platform.rye.ninja/cleanup`; the reconciler removes **only that** finalizer once (and, if the CR is already being deleted, stops there). The other six components keep theirs.
- Requeue: `Observing` and `Migrating` every 30 s; `NotConfigured`, `ReadyToRemoveLegacy` and `Verified` every 600 s.
- Leader gating and the shared `Context` are unchanged; standbys never write status.
- Every networked call uses `tokio::time::timeout_at`, never a bare `.await`.
- `cargo test` and `cargo clippy --all-targets` must pass (the CI commands); `deploy/crd.yaml` must equal `crds::generated_yaml()`; every commit message ends with exactly `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`.

## Review Focus

Failure modes the spec implies that most plausibly bite someone using this, and the task that owns each test:

1. **A node that cannot be reached or verified (TLS name rejected, timeout, error)** must never yield `ReadyToRemoveLegacy` or `Verified`. [Task 3, Task 5]
2. **Mixed writers during a rolling apiserver restart** must be `Observing`, never `Migrating` (a rewrite then could store Secrets under the old provider). [Task 3]
3. **A reader check that decrypts fewer objects than were listed (a cache-served list)** is incomplete and must never count as clean. [Task 3, Task 5]
4. **A counter reset (negative delta) or a vanished series mid-check** is "cannot verify", not "zero legacy reads". [Task 2, Task 5]
5. **A leftover v0.1.11 CR** (no `kmsProviderName`, carrying the old finalizer) is reported `InvalidSpec`, the finalizer is stripped, and deletion does not hang. [Task 1]
6. **Rewrite never runs** unless `rewrite: Enabled` and every node's writer is the target. [Task 3, Task 6]

---

### Task 1: New API types, strip the install flow, minimal reconciler, wiring

**Files:**
- Delete: `src/kms_provider.rs`, `src/kms_barbican.rs`, `src/talos_patches.rs`, `src/encryption_phase.rs`, `src/encryption_probe.rs`, `tests/integration_etcd_encryption.rs`
- Rewrite: `src/etcd_encryption.rs`, `src/etcd_encryption_reconciler.rs`, `examples/etcd-encryption.yaml`, `tests/etcd_encryption_example.rs`
- Modify: `src/lib.rs`, `src/main.rs`, `deploy/crd.yaml` (regenerated)

**Interfaces:**
- Consumes: `crate::crd::{Condition, PlatformKind}`, `crate::reconciler::{leader_gate, Context, SINGLETON_NAME}`.
- Produces, in `src/etcd_encryption.rs`:
  - `pub struct EtcdEncryptionSpec { pub platform_kind: PlatformKind, pub kms_provider_name: String, pub rewrite: RewriteMode, pub acknowledgements: Acknowledgements }` (CRD kind `EtcdEncryption`)
  - `pub enum RewriteMode { Disabled, Enabled }` (`Default` = `Disabled`, `Copy`)
  - `pub struct Acknowledgements { pub legacy_providers_removed: bool }` (`Default`, `Copy`)
  - `pub enum EncryptionPhase { Observing, NotConfigured, Migrating, ReadyToRemoveLegacy, Verified }` (`Default` = `Observing`, `Copy`)
  - `pub struct NodeStatus { pub name: String, pub address: String, pub verified: bool, pub writer_prefix: Option<String>, pub reads_by_prefix: BTreeMap<String, i64>, pub secrets_listed: i64, pub reason: String }`
  - `pub struct RewriteProgress { pub total: i64, pub rewritten: i64, pub failed: i64 }`
  - `pub struct EtcdEncryptionStatus { pub phase: EncryptionPhase, pub observed_generation: i64, pub legacy_prefixes: Vec<String>, pub nodes: Vec<NodeStatus>, pub rewrite: RewriteProgress, pub conditions: Vec<Condition> }`
  - `pub fn target_prefix(kms_provider_name: &str) -> String`
  - `pub enum EtcdEncryptionSpecError` with `pub fn reason(&self) -> &'static str`; `pub fn validate_etcd_encryption(spec: &EtcdEncryptionSpec) -> Result<(), EtcdEncryptionSpecError>`
- Produces, in `src/etcd_encryption_reconciler.rs` (still minimal here; Task 6 completes it): `ValidationError`, `validate(name, spec)`, `EtcdEncryptionReconcileError`, `condition(..)`, `strip_finalizer(&[String]) -> Option<Vec<String>>`, `write_status(..)`, `reconcile(obj, ctx)`, `error_policy(obj, err, ctx)`.

- [ ] **Step 1: Write the failing tests**

Replace `src/etcd_encryption.rs` with only this `#[cfg(test)]` module (the types do not exist yet, so it does not compile):

```rust
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
```

Replace `src/etcd_encryption_reconciler.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::{Acknowledgements, RewriteMode};

    fn spec() -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind: PlatformKind::TalosLinux,
            kms_provider_name: "barbican".to_string(),
            rewrite: RewriteMode::Disabled,
            acknowledgements: Acknowledgements::default(),
        }
    }

    #[test]
    fn accepts_talos_linux_named_default() {
        assert!(validate("default", &spec()).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let err = validate("second", &spec()).unwrap_err();

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec();
        spec.kms_provider_name = String::new();

        assert_eq!(validate("default", &spec).unwrap_err().reason(), "InvalidSpec");
    }

    #[test]
    fn strip_finalizer_removes_only_the_v0_1_11_finalizer() {
        // Review Focus 5.
        let finalizers = vec![
            "platform.rye.ninja/cleanup".to_string(),
            "other.example/keep".to_string(),
        ];

        assert_eq!(strip_finalizer(&finalizers), Some(vec!["other.example/keep".to_string()]));
    }

    #[test]
    fn strip_finalizer_is_none_when_ours_is_absent() {
        assert_eq!(strip_finalizer(&["other.example/keep".to_string()]), None);
        assert_eq!(strip_finalizer(&[]), None);
    }

    #[test]
    fn condition_is_true_only_when_requested() {
        let ok = condition("Ready", true, "Verified", "done", Some(3));
        let not_ok = condition("Ready", false, "Observing", "waiting", Some(3));

        assert_eq!((ok.status.as_str(), ok.observed_generation), ("True", Some(3)));
        assert_eq!(not_ok.status, "False");
    }

    #[test]
    fn failure_reasons_are_reported_only_where_status_may_be_overwritten() {
        assert_eq!(EtcdEncryptionReconcileError::NotLeader.failure_reason(), None);
        assert_eq!(
            EtcdEncryptionReconcileError::Validation(ValidationError::UnsupportedName("x".to_string())).failure_reason(),
            None,
            "validation already wrote its own status"
        );
        assert_eq!(
            EtcdEncryptionReconcileError::Verification("x".to_string()).failure_reason(),
            Some("VerificationFailed")
        );
    }
}
```

Replace `tests/etcd_encryption_example.rs` with:

```rust
use platform_controller::etcd_encryption::{EtcdEncryption, RewriteMode};
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
fn example_observes_only_by_default() {
    let spec = load().spec;

    assert_eq!(spec.rewrite, RewriteMode::Disabled);
    assert!(!spec.acknowledgements.legacy_providers_removed);
}
```

- [ ] **Step 2: Delete the install-flow modules and register the new set**

```bash
git rm -q src/kms_provider.rs src/kms_barbican.rs src/talos_patches.rs src/encryption_phase.rs src/encryption_probe.rs tests/integration_etcd_encryption.rs
```

In `src/lib.rs` delete the lines `pub mod encryption_phase;`, `pub mod kms_provider;`, `pub mod kms_barbican;`, `pub mod talos_patches;`, `pub mod encryption_probe;` (keep `etcd_encryption`, `etcd_encryption_reconciler`, `secret_rewrite`).

- [ ] **Step 3: Run to verify failure**

Run: `cargo test --lib etcd_encryption 2>&1 | tail -10`
Expected: FAIL to compile — `cannot find type EtcdEncryptionSpec` and others.

- [ ] **Step 4: Write the types**

Prepend to `src/etcd_encryption.rs`:

```rust
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
```

- [ ] **Step 5: Write the minimal reconciler**

Prepend to `src/etcd_encryption_reconciler.rs` (above its test module). It validates, strips the v0.1.11 finalizer, and reports `Observing`; Task 6 replaces the final status with real verification:

```rust
use crate::crd::{Condition, PlatformKind};
use crate::etcd_encryption::{
    EncryptionPhase, EtcdEncryption, EtcdEncryptionSpec, EtcdEncryptionSpecError, EtcdEncryptionStatus,
};
use crate::reconciler::{leader_gate, Context, SINGLETON_NAME};
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

/// The finalizer v0.1.11 added to every EtcdEncryption. This controller adds
/// none and owns nothing to clean up, so it removes this one (and only this one).
pub const LEGACY_FINALIZER: &str = "platform.rye.ninja/cleanup";

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
    #[error("kubernetes API call failed: {0}")]
    Api(#[source] kube::Error),
    /// The probe could not be built or the rewrite could not list Secrets.
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
    #[error("not the leader; standing down")]
    NotLeader,
}

impl EtcdEncryptionReconcileError {
    /// The `Ready=False` reason to report, or `None` when this error must not
    /// overwrite status. Exhaustive on purpose: a new variant forces a decision.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            EtcdEncryptionReconcileError::Api(_) => Some("ApiCallFailed"),
            EtcdEncryptionReconcileError::Verification(_) => Some("VerificationFailed"),
            EtcdEncryptionReconcileError::Validation(_)
            | EtcdEncryptionReconcileError::Status(_)
            | EtcdEncryptionReconcileError::NotLeader => None,
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

/// The finalizer list without the v0.1.11 finalizer, or `None` if it is absent.
pub fn strip_finalizer(finalizers: &[String]) -> Option<Vec<String>> {
    finalizers
        .iter()
        .any(|f| f == LEGACY_FINALIZER)
        .then(|| finalizers.iter().filter(|f| *f != LEGACY_FINALIZER).cloned().collect())
}

pub async fn write_status(
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

pub async fn reconcile(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }
    let name = obj.name_any();
    let generation = obj.metadata.generation;
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());

    // One-time migration: a CR created by v0.1.11 carries a finalizer this
    // controller no longer handles; leaving it would hang a pending deletion.
    if let Some(remaining) = strip_finalizer(obj.metadata.finalizers.as_deref().unwrap_or(&[])) {
        tracing::info!(installation = %name, "removing the v0.1.11 finalizer");
        let patch = serde_json::json!({ "metadata": { "finalizers": remaining } });
        api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
            .await
            .map_err(EtcdEncryptionReconcileError::Api)?;
        if obj.metadata.deletion_timestamp.is_some() {
            return Ok(Action::await_change());
        }
    }

    let mut status = obj.status.clone().unwrap_or_default();
    status.observed_generation = generation.unwrap_or(0);

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(installation = %name, error = %err, "validation failed");
        status.conditions = vec![condition("Ready", false, err.reason(), &err.to_string(), generation)];
        write_status(&api, &name, &status).await?;
        return Err(EtcdEncryptionReconcileError::Validation(err));
    }

    status.phase = EncryptionPhase::Observing;
    status.conditions = vec![condition(
        "Ready",
        false,
        "Observing",
        "spec is valid; apiserver verification is not wired up yet",
        generation,
    )];
    write_status(&api, &name, &status).await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

pub fn error_policy(
    _obj: Arc<EtcdEncryption>,
    _err: &EtcdEncryptionReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}
```

- [ ] **Step 6: Wire `main.rs` without the finalizer wrapper**

In `src/main.rs`, replace the etcd-encryption `Controller` block's `.run(...)` arguments:

```rust
        .run(
            etcd_encryption_reconciler::reconcile,
            etcd_encryption_reconciler::error_policy,
            context,
        )
```

(`reconcile` and `error_policy` now have the plain, non-finalizer signatures. Keep the watcher and its `predicates::generation.combine(deletion_requested).combine(predicates::finalizers)` filter exactly as is: the finalizer-strip path depends on seeing a CR that is being deleted.)

In the `main.rs` test `deletion_requested_works_for_the_etcd_encryption_kind_too`, replace the spec JSON with the new shape:

```rust
            "spec": {
                "platformKind": "talos-linux",
                "kmsProviderName": "barbican"
            }
```

- [ ] **Step 7: Replace the example**

`examples/etcd-encryption.yaml`:

```yaml
# Sample EtcdEncryption for a Talos Linux cluster whose apiserver already uses a
# KMS provider (for example the Barbican plugin run as Talos static pods).
#
# This controller does NOT install the KMS plugin, enable KMS, or edit your
# apiserver's EncryptionConfiguration. It observes each control-plane apiserver,
# can rewrite every Secret through the target provider, and proves per apiserver
# that nothing is left under a legacy provider (e.g. Talos's default secretbox)
# before telling you it is safe to remove it. See
# docs/runbooks/etcd-encryption-verification.md.
#
# EXPERIMENTAL: not yet verified against a live cluster. Not for clusters you
# care about.
#
# If this cluster ran EtcdEncryption v0.1.11, delete the old object and apply this
# one: the schema changed (provider, barbican and the acknowledgements were removed).
#
#   kubectl apply -f examples/etcd-encryption.yaml
#   kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}'
apiVersion: platform.rye.ninja/v1alpha1
kind: EtcdEncryption
metadata:
  name: default
spec:
  platformKind: talos-linux
  # The `name` of the kms entry in your apiserver's EncryptionConfiguration. The
  # target storage prefix is k8s:enc:kms:v2:<name>:.
  kmsProviderName: barbican
  # Disabled (default) only observes. Set Enabled to let the controller rewrite
  # every Secret, once every apiserver writes with the target provider.
  rewrite: Disabled
  acknowledgements:
    # Set true after you remove the legacy providers (e.g. secretbox) from the
    # apiserver's EncryptionConfiguration. Metrics cannot prove this; it is your
    # statement, and Verified means "the probes agree and you acknowledged it".
    legacyProvidersRemoved: false
```

- [ ] **Step 8: Regenerate the CRD manifest and run the tests**

Run: `cargo run -q --bin crdgen > deploy/crd.yaml && cargo test 2>&1 | grep -E "^test result|FAILED|failed" ; cargo clippy --all-targets 2>&1 | grep -c "^warning"`
Expected: every suite `ok`, `0 failed` (including `crd_yaml_matches_the_generated_crds`); no `uint64` anywhere in `deploy/crd.yaml` (`grep -c uint64 deploy/crd.yaml` prints `0`). If `tests/bootstrap_manifests.rs` fails on `etcdencryptions`, it is unchanged and should still pass.

- [ ] **Step 9: Commit**

```bash
git add -A src tests examples deploy
git commit -m "feat: EtcdEncryption adopt API (observe/migrate/verify); remove the install flow

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Metrics parsing and deltas (pure)

**Files:**
- Create: `src/transformation_metrics.rs`
- Modify: `src/lib.rs` (`pub mod transformation_metrics;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const TOP_LEVEL_PREFIX: &str = "k8s:enc:";`
  - `pub enum Direction { FromStorage, ToStorage }` (`Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash`)
  - `pub struct Transformations(pub BTreeMap<(Direction, String), i64>)` (`Clone, Debug, PartialEq, Eq, Default`)
  - `pub struct Delta { pub from_storage: BTreeMap<String, i64>, pub to_storage: BTreeMap<String, i64> }` (`Clone, Debug, PartialEq, Eq, Default`)
  - `pub fn parse_secret_transformations(metrics: &str) -> Transformations` — only `resource="secrets"`, `status="OK"` samples of `apiserver_storage_transformation_operations_total`, keeping prefixes that start with `k8s:enc:` or are empty (ignoring inner ones such as `key2:`); duplicate series are summed.
  - `pub fn delta(before: &Transformations, after: &Transformations) -> Option<Delta>` — only positive differences are kept; `None` if any series decreased or vanished (an apiserver restart reset the counters).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Taken from a real Talos cluster (2026-10-01), plus a HELP/TYPE header.
    const LIVE: &str = r#"# HELP apiserver_storage_transformation_operations_total [ALPHA] Total number of transformations.
# TYPE apiserver_storage_transformation_operations_total counter
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 284
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:secretbox:v1:"} 86
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="key2:"} 86
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="to_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 1
apiserver_request_total{code="200"} 5
"#;

    fn key(direction: Direction, prefix: &str) -> (Direction, String) {
        (direction, prefix.to_string())
    }

    #[test]
    fn parses_the_live_sample_and_ignores_the_inner_key_prefix() {
        let parsed = parse_secret_transformations(LIVE).0;

        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&key(Direction::FromStorage, "k8s:enc:kms:v2:barbican:")], 284);
        assert_eq!(parsed[&key(Direction::FromStorage, "k8s:enc:secretbox:v1:")], 86);
        assert_eq!(parsed[&key(Direction::ToStorage, "k8s:enc:kms:v2:barbican:")], 1);
        assert!(!parsed.contains_key(&key(Direction::FromStorage, "key2:")));
    }

    #[test]
    fn label_order_does_not_matter() {
        let line = r#"apiserver_storage_transformation_operations_total{transformer_prefix="k8s:enc:aescbc:v1:k:",transformation_type="from_storage",status="OK",resource="secrets"} 7"#;

        assert_eq!(
            parse_secret_transformations(line).0[&key(Direction::FromStorage, "k8s:enc:aescbc:v1:k:")],
            7
        );
    }

    #[test]
    fn other_resources_and_non_ok_statuses_are_ignored() {
        let text = r#"apiserver_storage_transformation_operations_total{resource="configmaps",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 9
apiserver_storage_transformation_operations_total{resource="secrets",status="Error",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 4"#;

        assert!(parse_secret_transformations(text).0.is_empty());
    }

    #[test]
    fn an_empty_prefix_is_kept_as_the_identity_reading() {
        let line = r#"apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix=""} 12"#;

        assert_eq!(parse_secret_transformations(line).0[&key(Direction::FromStorage, "")], 12);
    }

    #[test]
    fn an_empty_body_parses_to_nothing() {
        assert_eq!(parse_secret_transformations(""), Transformations::default());
    }

    #[test]
    fn delta_keeps_only_positive_differences_per_direction() {
        let before = parse_secret_transformations(LIVE);
        let after_text = LIVE
            .replace("transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 284", "transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 294")
            .replace("transformation_type=\"to_storage\",transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 1", "transformation_type=\"to_storage\",transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 2");
        let after = parse_secret_transformations(&after_text);

        let d = delta(&before, &after).expect("monotonic counters");

        assert_eq!(d.from_storage.len(), 1);
        assert_eq!(d.from_storage["k8s:enc:kms:v2:barbican:"], 10);
        assert_eq!(d.to_storage["k8s:enc:kms:v2:barbican:"], 1);
    }

    #[test]
    fn a_series_that_first_appears_counts_from_zero() {
        let before = Transformations::default();
        let after = parse_secret_transformations(LIVE);

        let d = delta(&before, &after).unwrap();

        assert_eq!(d.from_storage["k8s:enc:secretbox:v1:"], 86);
    }

    #[test]
    fn a_decreasing_counter_is_a_reset_not_a_delta() {
        // Review Focus 4: an apiserver restart mid-check.
        let before = parse_secret_transformations(LIVE);
        let after = parse_secret_transformations(&LIVE.replace("} 284", "} 3"));

        assert_eq!(delta(&before, &after), None);
    }

    #[test]
    fn a_vanished_series_is_a_reset_not_zero_reads() {
        // Review Focus 4: the counters disappeared (apiserver restarted).
        let before = parse_secret_transformations(LIVE);

        assert_eq!(delta(&before, &Transformations::default()), None);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib transformation_metrics 2>&1 | tail -8` (after adding the `pub mod` line)
Expected: FAIL to compile — `cannot find function parse_secret_transformations`.

- [ ] **Step 3: Implement**

Prepend to `src/transformation_metrics.rs`:

```rust
use std::collections::BTreeMap;

/// Top-level storage prefixes begin with this. Inner sub-prefixes (such as the
/// `key2:` seen under secretbox) describe the same operation and are ignored so
/// reads are not double-counted.
pub const TOP_LEVEL_PREFIX: &str = "k8s:enc:";

const FAMILY: &str = "apiserver_storage_transformation_operations_total";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    FromStorage,
    ToStorage,
}

/// Secrets transformation counters by `(direction, top-level prefix)`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Transformations(pub BTreeMap<(Direction, String), i64>);

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Delta {
    pub from_storage: BTreeMap<String, i64>,
    pub to_storage: BTreeMap<String, i64>,
}

/// Parses `key="value",key2="value2"` (the inside of a metric's braces).
fn parse_labels(labels: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = labels.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let Some(after) = after.strip_prefix('"') else { break };
        let Some(end) = after.find('"') else { break };
        out.insert(key, after[..end].to_string());
        rest = after[end + 1..].trim_start().trim_start_matches(',');
    }
    out
}

/// The Secrets `apiserver_storage_transformation_operations_total` counters
/// with `status="OK"`, keeping only top-level prefixes (`k8s:enc:...`) and the
/// empty prefix. Duplicate series are summed.
pub fn parse_secret_transformations(metrics: &str) -> Transformations {
    let mut out: BTreeMap<(Direction, String), i64> = BTreeMap::new();
    for line in metrics.lines() {
        let Some(rest) = line.strip_prefix(FAMILY) else { continue };
        let Some(rest) = rest.strip_prefix('{') else { continue };
        let Some(close) = rest.rfind('}') else { continue };
        let labels = parse_labels(&rest[..close]);
        if labels.get("resource").map(String::as_str) != Some("secrets")
            || labels.get("status").map(String::as_str) != Some("OK")
        {
            continue;
        }
        let direction = match labels.get("transformation_type").map(String::as_str) {
            Some("from_storage") => Direction::FromStorage,
            Some("to_storage") => Direction::ToStorage,
            _ => continue,
        };
        let prefix = labels.get("transformer_prefix").cloned().unwrap_or_default();
        if !(prefix.is_empty() || prefix.starts_with(TOP_LEVEL_PREFIX)) {
            continue;
        }
        let Ok(value) = rest[close + 1..].trim().parse::<f64>() else { continue };
        *out.entry((direction, prefix)).or_insert(0) += value.round() as i64;
    }
    Transformations(out)
}

/// The increase between two snapshots, positive entries only. `None` if any
/// series decreased or vanished: an apiserver restart reset its counters, so
/// the difference means nothing and must never be read as "no reads".
pub fn delta(before: &Transformations, after: &Transformations) -> Option<Delta> {
    if before.0.iter().any(|(k, b)| after.0.get(k).map_or(*b > 0, |a| a < b)) {
        return None;
    }
    let mut out = Delta::default();
    for ((direction, prefix), a) in &after.0 {
        let d = a - before.0.get(&(*direction, prefix.clone())).copied().unwrap_or(0);
        if d > 0 {
            match direction {
                Direction::FromStorage => out.from_storage.insert(prefix.clone(), d),
                Direction::ToStorage => out.to_storage.insert(prefix.clone(), d),
            };
        }
    }
    Some(out)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib transformation_metrics`
Expected: PASS (9 tests).

- [ ] **Step 5: Commit**

```bash
git add src/transformation_metrics.rs src/lib.rs
git commit -m "feat: parse apiserver transformation counters and deltas

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Per-node verdict and cluster derivation (pure)

**Files:**
- Create: `src/encryption_verdict.rs`
- Modify: `src/lib.rs` (`pub mod encryption_verdict;`), `docs/superpowers/specs/2026-10-01-etcd-encryption-adopt-design.md` (one rule amended, below)

**Interfaces:**
- Consumes: `EncryptionPhase, EtcdEncryptionSpec, NodeStatus, RewriteMode, target_prefix` (Task 1).
- Produces:
  - `pub enum Writer { Target, Other(Vec<String>), Unverifiable(String) }`
  - `pub struct Reader { pub listed: i64, pub reads: BTreeMap<String, i64> }` with `pub fn complete(&self) -> bool` (sum of reads ≥ listed) and `pub fn legacy_reads(&self, target: &str) -> i64`
  - `pub struct NodeEvidence { pub name: String, pub address: String, pub readyz_kms: bool, pub writer: Writer, pub reader: Option<Reader>, pub reader_error: Option<String> }`
  - `pub struct Derivation { pub phase: EncryptionPhase, pub run_rewrite: bool, pub reason: String, pub nodes: Vec<NodeStatus>, pub legacy_prefixes: Vec<String> }`
  - `pub fn all_clean(nodes: &[NodeEvidence], target: &str) -> bool` — non-empty, every writer `Target`, every reader present, complete, zero legacy reads
  - `pub fn derive(spec: &EtcdEncryptionSpec, nodes: &[NodeEvidence], canary_ok: bool) -> Derivation`

- [ ] **Step 1: Amend the spec's rule 4**

In `docs/superpowers/specs/2026-10-01-etcd-encryption-adopt-design.md`, in the derivation list item 4, replace the second sub-bullet (`otherwise legacy objects remain (or the check is incomplete) → ...`) with:

```
   - otherwise, if at least one node's reader check is complete with legacy reads
     and `rewrite: Enabled` → `Migrating` (run one rewrite pass); if the reader
     check is incomplete or unverifiable on every node, never rewrite (it would
     repeat full rewrites every 30 s while unable to confirm): `Observing` with
     "cannot verify reads"; otherwise `Observing` with "legacy objects remain; set
     `rewrite: Enabled` to migrate".
```

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::PlatformKind;
    use crate::etcd_encryption::{Acknowledgements, EncryptionPhase::*};

    const TARGET: &str = "k8s:enc:kms:v2:barbican:";
    const SECRETBOX: &str = "k8s:enc:secretbox:v1:";

    fn spec(rewrite: RewriteMode, acked: bool) -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind: PlatformKind::TalosLinux,
            kms_provider_name: "barbican".to_string(),
            rewrite,
            acknowledgements: Acknowledgements { legacy_providers_removed: acked },
        }
    }

    fn reader(listed: i64, kms: i64, secretbox: i64) -> Option<Reader> {
        let mut reads = BTreeMap::new();
        if kms > 0 {
            reads.insert(TARGET.to_string(), kms);
        }
        if secretbox > 0 {
            reads.insert(SECRETBOX.to_string(), secretbox);
        }
        Some(Reader { listed, reads })
    }

    fn node(name: &str, writer: Writer, reader: Option<Reader>) -> NodeEvidence {
        NodeEvidence {
            name: name.to_string(),
            address: "10.0.0.1".to_string(),
            readyz_kms: true,
            writer,
            reader,
            reader_error: None,
        }
    }

    fn clean(name: &str) -> NodeEvidence {
        node(name, Writer::Target, reader(10, 10, 0))
    }

    fn dirty(name: &str) -> NodeEvidence {
        node(name, Writer::Target, reader(10, 7, 3))
    }

    #[test]
    fn no_nodes_is_observing() {
        let d = derive(&spec(RewriteMode::Enabled, false), &[], false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
    }

    #[test]
    fn an_unverifiable_node_blocks_everything_even_when_the_others_are_clean() {
        // Review Focus 1.
        let nodes = [clean("a"), node("b", Writer::Unverifiable("tls: bad certificate".into()), None), clean("c")];

        let d = derive(&spec(RewriteMode::Enabled, true), &nodes, true);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert!(d.reason.contains("b") && d.reason.contains("tls: bad certificate"), "{}", d.reason);
        assert!(!d.nodes.iter().find(|n| n.name == "b").unwrap().verified);
    }

    #[test]
    fn no_writer_is_the_target_and_no_kms_reported_means_not_configured() {
        let mut a = node("a", Writer::Other(vec![SECRETBOX.to_string()]), None);
        a.readyz_kms = false;

        assert_eq!(derive(&spec(RewriteMode::Disabled, false), &[a], false).phase, NotConfigured);
    }

    #[test]
    fn kms_reported_but_not_writing_with_the_target_is_observing_with_a_hint() {
        let a = node("a", Writer::Other(vec![SECRETBOX.to_string()]), None);

        let d = derive(&spec(RewriteMode::Enabled, false), &[a], false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert!(d.reason.contains("kmsProviderName"), "{}", d.reason);
    }

    #[test]
    fn mixed_writers_never_migrate() {
        // Review Focus 2: a rolling restart; a rewrite now could store Secrets under the old provider.
        let nodes = [dirty("a"), node("b", Writer::Other(vec![SECRETBOX.to_string()]), None), dirty("c")];

        let d = derive(&spec(RewriteMode::Enabled, false), &nodes, false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert!(d.reason.contains("mixed"), "{}", d.reason);
    }

    #[test]
    fn every_node_clean_is_ready_to_remove_legacy_until_acknowledged() {
        let nodes = [clean("a"), clean("b"), clean("c")];

        let d = derive(&spec(RewriteMode::Disabled, false), &nodes, false);

        assert_eq!(d.phase, ReadyToRemoveLegacy);
        assert!(d.nodes.iter().all(|n| n.verified));
        assert!(d.legacy_prefixes.is_empty());
    }

    #[test]
    fn verified_needs_the_acknowledgement_and_a_round_tripping_canary() {
        let nodes = [clean("a"), clean("b")];

        assert_eq!(derive(&spec(RewriteMode::Disabled, true), &nodes, true).phase, Verified);
        assert_eq!(derive(&spec(RewriteMode::Disabled, true), &nodes, false).phase, ReadyToRemoveLegacy);
        assert_eq!(derive(&spec(RewriteMode::Disabled, false), &nodes, true).phase, ReadyToRemoveLegacy);
    }

    #[test]
    fn legacy_reads_with_rewrite_disabled_only_observe() {
        let nodes = [dirty("a"), clean("b")];

        let d = derive(&spec(RewriteMode::Disabled, false), &nodes, false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert_eq!(d.legacy_prefixes, vec![SECRETBOX.to_string()]);
        assert!(d.reason.contains("rewrite: Enabled"), "{}", d.reason);
    }

    #[test]
    fn legacy_reads_with_rewrite_enabled_and_every_writer_the_target_migrate() {
        // Review Focus 6.
        let nodes = [dirty("a"), clean("b"), clean("c")];

        let d = derive(&spec(RewriteMode::Enabled, false), &nodes, false);

        assert_eq!(d.phase, Migrating);
        assert!(d.run_rewrite);
    }

    #[test]
    fn a_cache_served_list_is_incomplete_and_never_counts_as_clean() {
        // Review Focus 3: 10 listed but only 4 decrypts seen.
        let nodes = [node("a", Writer::Target, reader(10, 4, 0)), clean("b")];

        assert!(!all_clean(&nodes, TARGET));
        let d = derive(&spec(RewriteMode::Enabled, true), &nodes, true);
        assert_ne!(d.phase, Verified);
        assert_ne!(d.phase, ReadyToRemoveLegacy);
    }

    #[test]
    fn an_incomplete_or_unverifiable_reader_never_triggers_a_rewrite() {
        let incomplete = [node("a", Writer::Target, reader(10, 4, 0))];
        let mut errored = node("a", Writer::Target, None);
        errored.reader_error = Some("list timed out".to_string());

        for nodes in [&incomplete[..], &[errored][..]] {
            let d = derive(&spec(RewriteMode::Enabled, false), nodes, false);

            assert_eq!(d.phase, Observing);
            assert!(!d.run_rewrite);
            assert!(d.reason.contains("cannot verify reads"), "{}", d.reason);
        }
    }

    #[test]
    fn an_identity_reading_is_legacy() {
        let mut reads = BTreeMap::new();
        reads.insert(TARGET.to_string(), 8);
        reads.insert(String::new(), 2);
        let nodes = [node("a", Writer::Target, Some(Reader { listed: 10, reads }))];

        assert!(!all_clean(&nodes, TARGET));
        assert_eq!(derive(&spec(RewriteMode::Disabled, false), &nodes, false).legacy_prefixes, vec![String::new()]);
    }

    #[test]
    fn all_clean_is_false_for_no_nodes() {
        assert!(!all_clean(&[], TARGET));
    }

    #[test]
    fn node_status_reports_the_writer_prefix_and_reads() {
        let d = derive(&spec(RewriteMode::Disabled, false), &[dirty("a")], false);
        let n = &d.nodes[0];

        assert_eq!(n.writer_prefix.as_deref(), Some(TARGET));
        assert_eq!(n.secrets_listed, 10);
        assert_eq!(n.reads_by_prefix[SECRETBOX], 3);
        assert!(!n.verified);
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test --lib encryption_verdict 2>&1 | tail -8` (after adding the `pub mod` line)
Expected: FAIL to compile — `cannot find type NodeEvidence`.

- [ ] **Step 4: Implement**

Prepend to `src/encryption_verdict.rs`:

```rust
use crate::etcd_encryption::{target_prefix, EncryptionPhase, EtcdEncryptionSpec, NodeStatus, RewriteMode};
use std::collections::{BTreeMap, BTreeSet};

/// Which provider an apiserver *writes* Secrets with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writer {
    Target,
    /// Writes happened but with other prefixes (or the target plus others).
    Other(Vec<String>),
    /// Could not be determined; the reason says why.
    Unverifiable(String),
}

/// What one apiserver *read* while listing every Secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reader {
    pub listed: i64,
    pub reads: BTreeMap<String, i64>,
}

impl Reader {
    /// Every listed object was decrypted at least once. A list served from the
    /// watch cache decrypts nothing, so it is never complete.
    pub fn complete(&self) -> bool {
        self.reads.values().sum::<i64>() >= self.listed
    }

    pub fn legacy_reads(&self, target: &str) -> i64 {
        self.reads.iter().filter(|(prefix, _)| prefix.as_str() != target).map(|(_, v)| *v).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeEvidence {
    pub name: String,
    pub address: String,
    /// `kms-providers` is present and ok in `/readyz?verbose`.
    pub readyz_kms: bool,
    pub writer: Writer,
    /// `None` when the reader check did not run or failed.
    pub reader: Option<Reader>,
    pub reader_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    pub phase: EncryptionPhase,
    /// Run one rewrite pass this reconcile.
    pub run_rewrite: bool,
    pub reason: String,
    pub nodes: Vec<NodeStatus>,
    pub legacy_prefixes: Vec<String>,
}

fn node_clean(node: &NodeEvidence, target: &str) -> bool {
    node.writer == Writer::Target
        && node.reader.as_ref().is_some_and(|r| r.complete() && r.legacy_reads(target) == 0)
}

/// Every node writes with the target and read every Secret with zero legacy
/// reads. False for no nodes.
pub fn all_clean(nodes: &[NodeEvidence], target: &str) -> bool {
    !nodes.is_empty() && nodes.iter().all(|n| node_clean(n, target))
}

fn node_status(node: &NodeEvidence, target: &str) -> NodeStatus {
    let (writer_prefix, mut reason) = match &node.writer {
        Writer::Target => (Some(target.to_string()), String::new()),
        Writer::Other(prefixes) => (prefixes.first().cloned(), format!("writes with {prefixes:?}, not the target")),
        Writer::Unverifiable(why) => (None, why.clone()),
    };
    let (reads_by_prefix, secrets_listed) = match &node.reader {
        Some(r) => (r.reads.clone(), r.listed),
        None => (BTreeMap::new(), 0),
    };
    if let Some(r) = &node.reader {
        if !r.complete() {
            reason = "cannot verify reads: fewer objects were decrypted than listed".to_string();
        } else if r.legacy_reads(target) > 0 {
            reason = format!("{} legacy reads", r.legacy_reads(target));
        }
    }
    if let Some(err) = &node.reader_error {
        reason = format!("cannot verify reads: {err}");
    }
    NodeStatus {
        name: node.name.clone(),
        address: node.address.clone(),
        verified: node_clean(node, target),
        writer_prefix,
        reads_by_prefix,
        secrets_listed,
        reason,
    }
}

fn derivation(
    phase: EncryptionPhase,
    run_rewrite: bool,
    reason: impl Into<String>,
    statuses: Vec<NodeStatus>,
    legacy: Vec<String>,
) -> Derivation {
    Derivation { phase, run_rewrite, reason: reason.into(), nodes: statuses, legacy_prefixes: legacy }
}

/// The phase, derived from this reconcile's evidence and the spec alone. No
/// remembered state, so nothing can regress or go stale.
pub fn derive(spec: &EtcdEncryptionSpec, nodes: &[NodeEvidence], canary_ok: bool) -> Derivation {
    use EncryptionPhase::*;
    let target = target_prefix(&spec.kms_provider_name);
    let statuses: Vec<NodeStatus> = nodes.iter().map(|n| node_status(n, &target)).collect();
    let legacy: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.reader.as_ref())
        .flat_map(|r| r.reads.iter())
        .filter(|(prefix, count)| prefix.as_str() != target && **count > 0)
        .map(|(prefix, _)| prefix.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let out = |phase, run_rewrite, reason: &str| {
        derivation(phase, run_rewrite, reason, statuses.clone(), legacy.clone())
    };

    if nodes.is_empty() {
        return out(Observing, false, "no control-plane nodes found");
    }
    if let Some((node, why)) = nodes.iter().find_map(|n| match &n.writer {
        Writer::Unverifiable(why) => Some((n, why)),
        _ => None,
    }) {
        return out(Observing, false, &format!("cannot verify node {}: {why}", node.name));
    }

    let writers_target = nodes.iter().filter(|n| n.writer == Writer::Target).count();
    if writers_target == 0 {
        return if nodes.iter().any(|n| n.readyz_kms) {
            out(
                Observing,
                false,
                &format!(
                    "a KMS provider is configured but the apiserver is not writing with {target}: check \
                     spec.kmsProviderName and that the KMS provider is listed first"
                ),
            )
        } else {
            out(
                NotConfigured,
                false,
                "no KMS provider is active; this controller does not enable KMS (see the runbook)",
            )
        };
    }
    if writers_target < nodes.len() {
        return out(Observing, false, "mixed: some apiservers do not write with the target provider yet (a rolling restart?)");
    }

    // Every apiserver writes with the target provider.
    if all_clean(nodes, &target) {
        return if spec.acknowledgements.legacy_providers_removed && canary_ok {
            out(
                Verified,
                false,
                "every apiserver reads and writes only with the target provider, and you acknowledged \
                 removing the legacy providers (metrics cannot prove the config no longer lists them)",
            )
        } else if spec.acknowledgements.legacy_providers_removed {
            out(ReadyToRemoveLegacy, false, "the canary Secret did not round-trip; not verified")
        } else {
            out(
                ReadyToRemoveLegacy,
                false,
                "no Secret is stored under a legacy provider on any apiserver: it is safe to remove the \
                 legacy providers from the EncryptionConfiguration, then set \
                 acknowledgements.legacyProvidersRemoved",
            )
        };
    }

    let legacy_seen = nodes
        .iter()
        .filter_map(|n| n.reader.as_ref())
        .any(|r| r.complete() && r.legacy_reads(&target) > 0);
    if legacy_seen && spec.rewrite == RewriteMode::Enabled {
        return out(Migrating, true, "rewriting every Secret through the target provider");
    }
    if legacy_seen {
        return out(Observing, false, "legacy objects remain; set rewrite: Enabled to migrate");
    }
    out(Observing, false, "cannot verify reads on every apiserver; not rewriting")
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test --lib encryption_verdict`
Expected: PASS (14 tests).

- [ ] **Step 6: Commit**

```bash
git add src/encryption_verdict.rs src/lib.rs docs/superpowers/specs/2026-10-01-etcd-encryption-adopt-design.md
git commit -m "feat: derive the EtcdEncryption phase from per-apiserver evidence

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 4: The apiserver probe (trait, helpers, real per-node client)

**Files:**
- Create: `src/apiserver_probe.rs`
- Modify: `src/lib.rs` (`pub mod apiserver_probe;`)

**Interfaces:**
- Consumes: `crate::secret_rewrite::{KubeSecretStore, verify_listable}`.
- Produces:
  - `pub const APISERVER_PORT: u16 = 6443;` `pub const TLS_SERVER_NAME: &str = "kubernetes.default.svc";` `pub const CANARY_NAME: &str = "etcd-encryption-canary";` `pub const CANARY_NAMESPACE: &str = "kube-system";`
  - `pub struct NodeTarget { pub name: String, pub address: String }` (`Clone, Debug, PartialEq, Eq`)
  - `#[derive(thiserror::Error, Debug)] pub enum ProbeError { Request(String), Timeout(Duration) }`
  - `pub trait ApiserverProbe` (`#[allow(async_fn_in_trait)]`):
    `async fn control_plane_nodes(&self) -> Result<Vec<NodeTarget>, ProbeError>;`
    `async fn readyz_kms(&self, node: &NodeTarget) -> Result<bool, ProbeError>;`
    `async fn metrics(&self, node: &NodeTarget) -> Result<String, ProbeError>;`
    `async fn canary_write(&self, node: &NodeTarget) -> Result<(), ProbeError>;`
    `async fn list_all_secrets(&self, node: &NodeTarget) -> Result<u64, ProbeError>;`
    `async fn canary_round_trip(&self) -> Result<bool, ProbeError>;`
  - pure: `pub fn kms_check_ok(readyz_body: &str) -> bool`, `pub fn apiserver_url(address: &str) -> String`, `pub fn node_targets(nodes: &[Node]) -> Vec<NodeTarget>`, `pub fn canary_secret(value: &str) -> Secret`
  - `pub struct KubeApiserverProbe` with `pub fn new(client: kube::Client) -> Result<Self, ProbeError>` implementing `ApiserverProbe`; `pub async fn delete_canary(client: &kube::Client)` (best effort)

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib apiserver_probe 2>&1 | tail -8` (after adding the `pub mod` line)
Expected: FAIL to compile — `cannot find function kms_check_ok`.

- [ ] **Step 3: Implement**

Prepend to `src/apiserver_probe.rs`:

```rust
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
```

> If `Node.status.addresses[].type_` or `Config::incluster()`'s error type differ in kube 4.2 / k8s-openapi 0.28, adapt the field names (the compiler will say); the pure helpers' tests do not change. If `ApiserverProbe` futures must be `Send` for the controller, add a `Send` bound at the use site rather than boxing.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib apiserver_probe && cargo clippy --all-targets 2>&1 | tail -5`
Expected: PASS (6 tests); no new warning classes.

- [ ] **Step 5: Commit**

```bash
git add src/apiserver_probe.rs src/lib.rs
git commit -m "feat: per-apiserver probe trait and real client

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Verification orchestration (against a fake probe)

**Files:**
- Create: `src/encryption_verify.rs`
- Modify: `src/lib.rs` (`pub mod encryption_verify;`)

**Interfaces:**
- Consumes: `ApiserverProbe, NodeTarget, ProbeError` (Task 4); `parse_secret_transformations, delta, Direction` (Task 2); `NodeEvidence, Writer, Reader` (Task 3).
- Produces:
  - `pub async fn verify_node<P: ApiserverProbe>(probe: &P, node: &NodeTarget, target: &str) -> NodeEvidence`
  - `pub async fn verify_cluster<P: ApiserverProbe>(probe: &P, target: &str) -> Result<Vec<NodeEvidence>, ProbeError>`

Per node, in this exact call order: `readyz_kms`, `metrics`, `canary_write`, `metrics`; then, only if the writer is `Target`: `metrics`, `list_all_secrets`, `metrics`. Writer: target iff the `to_storage` delta has a positive entry for `target` and no positive entry for any other prefix; other positive prefixes → `Other`; no positive entry or a `None` delta → `Unverifiable`. Any probe error in readyz/metrics/canary write → `Unverifiable` (the error text). Reader errors → `reader_error`, `reader: None`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const TARGET: &str = "k8s:enc:kms:v2:barbican:";
    const SECRETBOX: &str = "k8s:enc:secretbox:v1:";

    fn metrics(rows: &[(&str, &str, i64)]) -> String {
        rows.iter()
            .map(|(dir, prefix, n)| {
                format!(
                    "apiserver_storage_transformation_operations_total{{resource=\"secrets\",status=\"OK\",transformation_type=\"{dir}\",transformer_prefix=\"{prefix}\"}} {n}\n"
                )
            })
            .collect()
    }

    #[derive(Default)]
    struct Script {
        readyz: Option<Result<bool, String>>,
        metrics: Vec<Result<String, String>>,
        canary_write: Option<Result<(), String>>,
        list: Option<Result<u64, String>>,
    }

    struct Fake {
        nodes: Result<Vec<NodeTarget>, String>,
        scripts: Mutex<HashMap<String, Script>>,
    }

    fn target(name: &str) -> NodeTarget {
        NodeTarget { name: name.to_string(), address: format!("10.0.0.{}", name.len()) }
    }

    impl Fake {
        fn with(scripts: Vec<(&str, Script)>) -> Fake {
            Fake {
                nodes: Ok(scripts.iter().map(|(n, _)| target(n)).collect()),
                scripts: Mutex::new(scripts.into_iter().map(|(n, s)| (n.to_string(), s)).collect()),
            }
        }
    }

    impl ApiserverProbe for Fake {
        async fn control_plane_nodes(&self) -> Result<Vec<NodeTarget>, ProbeError> {
            self.nodes.clone().map_err(ProbeError::Request)
        }
        async fn readyz_kms(&self, node: &NodeTarget) -> Result<bool, ProbeError> {
            self.scripts.lock().unwrap().get_mut(&node.name).unwrap().readyz.take().unwrap().map_err(ProbeError::Request)
        }
        async fn metrics(&self, node: &NodeTarget) -> Result<String, ProbeError> {
            let mut scripts = self.scripts.lock().unwrap();
            let s = scripts.get_mut(&node.name).unwrap();
            assert!(!s.metrics.is_empty(), "unexpected extra metrics call on {}", node.name);
            s.metrics.remove(0).map_err(ProbeError::Request)
        }
        async fn canary_write(&self, node: &NodeTarget) -> Result<(), ProbeError> {
            self.scripts.lock().unwrap().get_mut(&node.name).unwrap().canary_write.take().unwrap().map_err(ProbeError::Request)
        }
        async fn list_all_secrets(&self, node: &NodeTarget) -> Result<u64, ProbeError> {
            self.scripts.lock().unwrap().get_mut(&node.name).unwrap().list.take().unwrap().map_err(ProbeError::Request)
        }
        async fn canary_round_trip(&self) -> Result<bool, ProbeError> {
            Ok(true)
        }
    }

    /// A healthy node: writes with the target; the list decrypts 7 target + 3 secretbox of 10.
    fn healthy(reads_kms: i64, reads_secretbox: i64, listed: u64) -> Script {
        Script {
            readyz: Some(Ok(true)),
            metrics: vec![
                Ok(metrics(&[("to_storage", TARGET, 1), ("from_storage", TARGET, 100), ("from_storage", SECRETBOX, 50)])),
                Ok(metrics(&[("to_storage", TARGET, 2), ("from_storage", TARGET, 100), ("from_storage", SECRETBOX, 50)])),
                Ok(metrics(&[("to_storage", TARGET, 2), ("from_storage", TARGET, 100), ("from_storage", SECRETBOX, 50)])),
                Ok(metrics(&[
                    ("to_storage", TARGET, 2),
                    ("from_storage", TARGET, 100 + reads_kms),
                    ("from_storage", SECRETBOX, 50 + reads_secretbox),
                ])),
            ],
            canary_write: Some(Ok(())),
            list: Some(Ok(listed)),
        }
    }

    #[tokio::test]
    async fn a_healthy_node_writes_with_the_target_and_reports_its_reads() {
        let fake = Fake::with(vec![("cp1", healthy(7, 3, 10))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.readyz_kms);
        let r = e.reader.expect("reader ran");
        assert_eq!(r.listed, 10);
        assert_eq!(r.reads[TARGET], 7);
        assert_eq!(r.reads[SECRETBOX], 3);
        assert!(r.complete());
    }

    #[tokio::test]
    async fn a_cache_served_list_decrypts_fewer_objects_than_listed() {
        // Review Focus 3.
        let fake = Fake::with(vec![("cp1", healthy(2, 0, 10))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(!e.reader.unwrap().complete());
    }

    #[tokio::test]
    async fn a_node_writing_with_another_provider_is_not_the_target_and_skips_the_reader() {
        let mut s = healthy(7, 3, 10);
        s.metrics[1] = Ok(metrics(&[("to_storage", TARGET, 1), ("to_storage", SECRETBOX, 1), ("from_storage", TARGET, 100)]));
        s.metrics[0] = Ok(metrics(&[("to_storage", TARGET, 1), ("from_storage", TARGET, 100)]));
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Other(vec![SECRETBOX.to_string()]));
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn a_counter_reset_between_snapshots_is_unverifiable() {
        // Review Focus 4.
        let mut s = healthy(7, 3, 10);
        s.metrics[1] = Ok(metrics(&[("to_storage", TARGET, 0), ("from_storage", TARGET, 1)]));
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(matches!(e.writer, Writer::Unverifiable(_)), "{:?}", e.writer);
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn no_observed_write_is_unverifiable() {
        let mut s = healthy(7, 3, 10);
        s.metrics[1] = s.metrics[0].clone();
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        assert!(matches!(verify_node(&fake, &target("cp1"), TARGET).await.writer, Writer::Unverifiable(_)));
    }

    #[tokio::test]
    async fn a_readyz_error_makes_the_node_unverifiable_and_stops() {
        // Review Focus 1.
        let s = Script { readyz: Some(Err("tls: bad certificate".to_string())), ..Default::default() };
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(matches!(&e.writer, Writer::Unverifiable(why) if why.contains("bad certificate")));
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn a_canary_write_error_makes_the_node_unverifiable() {
        let mut s = healthy(7, 3, 10);
        s.canary_write = Some(Err("forbidden".to_string()));
        s.metrics.truncate(1);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        assert!(matches!(verify_node(&fake, &target("cp1"), TARGET).await.writer, Writer::Unverifiable(_)));
    }

    #[tokio::test]
    async fn a_list_error_is_a_reader_error_not_a_clean_result() {
        let mut s = healthy(7, 3, 10);
        s.list = Some(Err("timed out".to_string()));
        s.metrics.truncate(3);
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.reader.is_none());
        assert!(e.reader_error.as_deref().is_some_and(|m| m.contains("timed out")));
    }

    #[tokio::test]
    async fn verify_cluster_checks_every_discovered_node() {
        let fake = Fake::with(vec![("cp1", healthy(10, 0, 10)), ("cp22", healthy(10, 0, 10))]);

        let evidence = verify_cluster(&fake, TARGET).await.unwrap();

        assert_eq!(evidence.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["cp1", "cp22"]);
        assert!(evidence.iter().all(|e| e.writer == Writer::Target));
    }

    #[tokio::test]
    async fn a_node_discovery_error_is_an_error_not_an_empty_result() {
        let fake = Fake { nodes: Err("list nodes: forbidden".to_string()), scripts: Mutex::new(HashMap::new()) };

        assert!(verify_cluster(&fake, TARGET).await.is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib encryption_verify 2>&1 | tail -8` (after adding the `pub mod` line)
Expected: FAIL to compile — `cannot find function verify_node`.

- [ ] **Step 3: Implement**

Prepend to `src/encryption_verify.rs`:

```rust
use crate::apiserver_probe::{ApiserverProbe, NodeTarget, ProbeError};
use crate::encryption_verdict::{NodeEvidence, Reader, Writer};
use crate::transformation_metrics::{delta, parse_secret_transformations, Delta};

fn counters(body: &str) -> crate::transformation_metrics::Transformations {
    parse_secret_transformations(body)
}

/// Which provider this apiserver wrote with, from the `to_storage` delta around
/// one canary write.
fn writer_from(delta: Option<Delta>, target: &str) -> Writer {
    let Some(delta) = delta else {
        return Writer::Unverifiable("a counter went backwards (the apiserver restarted mid-check)".to_string());
    };
    let others: Vec<String> = delta.to_storage.keys().filter(|p| p.as_str() != target).cloned().collect();
    if !others.is_empty() {
        return Writer::Other(others);
    }
    if delta.to_storage.get(target).copied().unwrap_or(0) > 0 {
        Writer::Target
    } else {
        Writer::Unverifiable("the canary write was not counted by any provider".to_string())
    }
}

async fn read_check<P: ApiserverProbe>(probe: &P, node: &NodeTarget) -> Result<Reader, String> {
    let before = counters(&probe.metrics(node).await.map_err(|e| e.to_string())?);
    let listed = probe.list_all_secrets(node).await.map_err(|e| e.to_string())?;
    let after = counters(&probe.metrics(node).await.map_err(|e| e.to_string())?);
    let delta = delta(&before, &after)
        .ok_or_else(|| "a counter went backwards (the apiserver restarted mid-check)".to_string())?;
    Ok(Reader { listed: listed as i64, reads: delta.from_storage })
}

/// Verifies one apiserver. Any failure leaves the node unverified with the
/// reason; nothing here can turn an error into a clean result.
pub async fn verify_node<P: ApiserverProbe>(probe: &P, node: &NodeTarget, target: &str) -> NodeEvidence {
    let evidence = |readyz_kms, writer, reader, reader_error| NodeEvidence {
        name: node.name.clone(),
        address: node.address.clone(),
        readyz_kms,
        writer,
        reader,
        reader_error,
    };
    let unverifiable = |readyz_kms: bool, why: String| evidence(readyz_kms, Writer::Unverifiable(why), None, None);

    let readyz_kms = match probe.readyz_kms(node).await {
        Ok(v) => v,
        Err(err) => return unverifiable(false, err.to_string()),
    };

    let before = match probe.metrics(node).await {
        Ok(body) => counters(&body),
        Err(err) => return unverifiable(readyz_kms, err.to_string()),
    };
    if let Err(err) = probe.canary_write(node).await {
        return unverifiable(readyz_kms, format!("canary write failed: {err}"));
    }
    let after = match probe.metrics(node).await {
        Ok(body) => counters(&body),
        Err(err) => return unverifiable(readyz_kms, err.to_string()),
    };

    let writer = writer_from(delta(&before, &after), target);
    if writer != Writer::Target {
        return evidence(readyz_kms, writer, None, None);
    }
    match read_check(probe, node).await {
        Ok(reader) => evidence(readyz_kms, writer, Some(reader), None),
        Err(err) => evidence(readyz_kms, writer, None, Some(err)),
    }
}

/// Verifies every control-plane apiserver, in discovery order. A discovery error
/// is an error: an empty result must never be mistaken for "no nodes to worry about".
pub async fn verify_cluster<P: ApiserverProbe>(probe: &P, target: &str) -> Result<Vec<NodeEvidence>, ProbeError> {
    let nodes = probe.control_plane_nodes().await?;
    let mut evidence = Vec::with_capacity(nodes.len());
    for node in &nodes {
        evidence.push(verify_node(probe, node, target).await);
    }
    Ok(evidence)
}
```

> The test `Fake` clones `Result<Vec<NodeTarget>, String>` and unwraps scripted `Option`s with `.take().unwrap()` — those unwraps are test-only and assert the documented call order (an unexpected extra call fails the test loudly).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib encryption_verify`
Expected: PASS (10 tests).

- [ ] **Step 5: Commit**

```bash
git add src/encryption_verify.rs src/lib.rs
git commit -m "feat: verify each control-plane apiserver (writer and reader checks)

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 6: The reconciler (verify, derive, rewrite, status)

**Files:**
- Modify: `src/etcd_encryption_reconciler.rs`

**Interfaces:**
- Consumes: Tasks 1-5 (`verify_cluster`, `derive`, `all_clean`, `KubeApiserverProbe`, `delete_canary`, `rewrite_page`, `KubeSecretStore`), existing `crate::reconciler::{leader_gate, Context}`.
- Produces: the final `reconcile`, plus pure `requeue_for(phase) -> Duration`, `ready_condition_for(&Derivation, Option<i64>) -> Condition`, `run_rewrite(..)`.

The Task 1 reconciler already validates and strips the finalizer; this task replaces its tail (the `Observing` placeholder) with the real flow.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/etcd_encryption_reconciler.rs`:

```rust
    use crate::encryption_verdict::Derivation;
    use crate::etcd_encryption::EncryptionPhase;

    fn derivation(phase: EncryptionPhase) -> Derivation {
        Derivation {
            phase,
            run_rewrite: false,
            reason: "why".to_string(),
            nodes: vec![],
            legacy_prefixes: vec![],
        }
    }

    #[test]
    fn active_phases_requeue_quickly_and_settled_ones_slowly() {
        use EncryptionPhase::*;
        assert_eq!(requeue_for(Observing), Duration::from_secs(30));
        assert_eq!(requeue_for(Migrating), Duration::from_secs(30));
        assert_eq!(requeue_for(NotConfigured), Duration::from_secs(600));
        assert_eq!(requeue_for(ReadyToRemoveLegacy), Duration::from_secs(600));
        assert_eq!(requeue_for(Verified), Duration::from_secs(600));
    }

    #[test]
    fn ready_is_true_only_for_verified() {
        for (phase, ready) in [
            (EncryptionPhase::Observing, false),
            (EncryptionPhase::NotConfigured, false),
            (EncryptionPhase::Migrating, false),
            (EncryptionPhase::ReadyToRemoveLegacy, false),
            (EncryptionPhase::Verified, true),
        ] {
            let c = ready_condition_for(&derivation(phase), Some(2));

            assert_eq!(c.status == "True", ready, "{phase:?}");
            assert_eq!(c.reason, format!("{phase:?}"));
            assert_eq!(c.message, "why");
        }
    }

    #[test]
    fn rewrite_only_runs_when_the_derivation_asks_for_it_and_the_spec_enables_it() {
        // Review Focus 6: belt and braces on top of derive().
        let mut spec = spec();
        let mut d = derivation(EncryptionPhase::Migrating);
        d.run_rewrite = true;

        spec.rewrite = RewriteMode::Disabled;
        assert!(!should_run_rewrite(&spec, &d));

        spec.rewrite = RewriteMode::Enabled;
        assert!(should_run_rewrite(&spec, &d));

        d.run_rewrite = false;
        assert!(!should_run_rewrite(&spec, &d));

        let mut wrong_phase = derivation(EncryptionPhase::Observing);
        wrong_phase.run_rewrite = true;
        assert!(!should_run_rewrite(&spec, &wrong_phase));
    }
```

(The Task 1 test `only_a_database_error_is_reported_as_a_failure_reason` stays.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib etcd_encryption_reconciler 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find function requeue_for`.

- [ ] **Step 3: Implement**

In `src/etcd_encryption_reconciler.rs` add these imports at the top:

```rust
use crate::apiserver_probe::{delete_canary, KubeApiserverProbe};
use crate::encryption_verdict::{all_clean, derive, Derivation};
use crate::etcd_encryption::{target_prefix, RewriteMode, RewriteProgress};
use crate::secret_rewrite::{rewrite_page, KubeSecretStore};
```

Add these functions:

```rust
/// Active phases are re-checked quickly; settled ones slowly (the reader check
/// lists every Secret on every control-plane apiserver, so it is not cheap).
pub fn requeue_for(phase: EncryptionPhase) -> Duration {
    match phase {
        EncryptionPhase::Observing | EncryptionPhase::Migrating => Duration::from_secs(30),
        EncryptionPhase::NotConfigured | EncryptionPhase::ReadyToRemoveLegacy | EncryptionPhase::Verified => {
            Duration::from_secs(600)
        }
    }
}

/// `Ready` is true only when the cluster is `Verified`.
pub fn ready_condition_for(derivation: &Derivation, generation: Option<i64>) -> Condition {
    condition(
        "Ready",
        derivation.phase == EncryptionPhase::Verified,
        &format!("{:?}", derivation.phase),
        &derivation.reason,
        generation,
    )
}

/// The rewrite runs only when the derivation says so for the `Migrating` phase
/// AND the spec enables it. `derive` already requires both; this is the second lock.
pub fn should_run_rewrite(spec: &EtcdEncryptionSpec, derivation: &Derivation) -> bool {
    derivation.run_rewrite
        && derivation.phase == EncryptionPhase::Migrating
        && spec.rewrite == RewriteMode::Enabled
}

/// Rewrites every Secret, writing progress to status after each page. Failures
/// are reported by namespace/name only, never by data.
async fn run_rewrite(
    api: &Api<EtcdEncryption>,
    name: &str,
    store: &KubeSecretStore,
    status: &mut EtcdEncryptionStatus,
) -> Result<(), EtcdEncryptionReconcileError> {
    let mut progress = RewriteProgress::default();
    let mut token: Option<String> = None;
    loop {
        let page = rewrite_page(store, token.as_deref())
            .await
            .map_err(|err| EtcdEncryptionReconcileError::Verification(err.to_string()))?;
        progress.total += page.seen as i64;
        progress.rewritten += page.rewritten as i64;
        progress.failed += page.failed.len() as i64;
        for key in &page.failed {
            tracing::warn!(namespace = %key.namespace, secret = %key.name, "failed to rewrite secret");
        }
        status.rewrite = progress.clone();
        write_status(api, name, status).await?;
        match page.next {
            Some(next) => token = Some(next),
            None => return Ok(()),
        }
    }
}
```

Replace the tail of `reconcile` (everything after the validation block, i.e. the `status.phase = EncryptionPhase::Observing; ... Ok(Action::requeue(...))` placeholder) with:

```rust
    let target = target_prefix(&obj.spec.kms_provider_name);
    let probe = KubeApiserverProbe::new(ctx.client.clone())
        .map_err(|err| EtcdEncryptionReconcileError::Verification(err.to_string()))?;

    // Verify every control-plane apiserver. A discovery error is an error, never "no nodes".
    let evidence = match crate::encryption_verify::verify_cluster(&probe, &target).await {
        Ok(evidence) => evidence,
        Err(err) => {
            status.conditions = vec![condition(
                "Ready",
                false,
                "Observing",
                &format!("cannot discover or verify the control-plane apiservers: {err}"),
                generation,
            )];
            status.phase = EncryptionPhase::Observing;
            write_status(&api, &name, &status).await?;
            return Ok(Action::requeue(requeue_for(EncryptionPhase::Observing)));
        }
    };

    // The canary round trip is only worth doing when everything else is clean.
    let canary_ok = if all_clean(&evidence, &target) && obj.spec.acknowledgements.legacy_providers_removed {
        probe.canary_round_trip().await.unwrap_or_else(|err| {
            tracing::warn!(error = %err, "canary round trip failed");
            false
        })
    } else {
        false
    };

    let derivation = derive(&obj.spec, &evidence, canary_ok);
    tracing::info!(installation = %name, phase = ?derivation.phase, reason = %derivation.reason, "derived phase");

    status.phase = derivation.phase;
    status.legacy_prefixes = derivation.legacy_prefixes.clone();
    status.nodes = derivation.nodes.clone();
    status.conditions = vec![ready_condition_for(&derivation, generation)];
    write_status(&api, &name, &status).await?;

    if should_run_rewrite(&obj.spec, &derivation) {
        let store = KubeSecretStore::new(ctx.client.clone());
        run_rewrite(&api, &name, &store, &mut status).await?;
    }

    // The canary is a probe, not state; do not leave it behind once settled.
    if matches!(derivation.phase, EncryptionPhase::ReadyToRemoveLegacy | EncryptionPhase::Verified) {
        delete_canary(&ctx.client).await;
    }

    Ok(Action::requeue(requeue_for(derivation.phase)))
```

Remove the now-unused `EtcdEncryptionSpecError` import if clippy flags it.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib etcd_encryption_reconciler && cargo test 2>&1 | grep -E "^test result|FAILED|failed" ; cargo clippy --all-targets 2>&1 | tail -5`
Expected: all suites `ok`, `0 failed`; no new warning classes.

- [ ] **Step 5: Commit**

```bash
git add src/etcd_encryption_reconciler.rs
git commit -m "feat: EtcdEncryption reconciler verifies, derives, rewrites and reports

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Runbook, integration test, README, memory and RBAC ledger

**Files:**
- Rewrite: `docs/runbooks/etcd-encryption-verification.md`
- Create: `tests/integration_etcd_encryption.rs`
- Modify: `deploy/README.md`, `docs/memory/etcd-encryption-2026-09.md`, `docs/memory/MEMORY.md`, `docs/memory/rbac-cluster-admin-tradeoff.md`

**Interfaces:**
- Consumes: the finished behaviour of Tasks 1-6 (read `src/encryption_verdict.rs` and `src/etcd_encryption_reconciler.rs` so the docs match the code).

- [ ] **Step 1: Write the ignored integration test**

`tests/integration_etcd_encryption.rs`:

```rust
// Run manually against a real cluster whose apiserver already uses a KMS
// provider (see docs/runbooks/etcd-encryption-verification.md). With the
// controller running and all seven CRDs Established:
//
//   kubectl apply -f examples/etcd-encryption.yaml
//   cargo test --test integration_etcd_encryption -- --ignored --nocapture
//
// This checks only what the controller can do by itself and never rewrites
// (the example leaves rewrite: Disabled): it must reach a phase other than the
// initial one and report one status entry per control-plane node. It changes
// nothing in the cluster apart from a canary Secret in kube-system.

use kube::api::Api;
use kube::Client;
use platform_controller::etcd_encryption::EtcdEncryption;
use std::time::Duration;

#[tokio::test]
#[ignore = "needs a live cluster with the controller running; see the header comment"]
async fn observes_every_control_plane_apiserver() {
    let client = Client::try_default().await.expect("a kubeconfig for the test cluster");
    let encryptions: Api<EtcdEncryption> = Api::all(client.clone());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let status = loop {
        let current = encryptions.get("default").await.expect("apply examples/etcd-encryption.yaml first");
        if let Some(status) = current.status.filter(|s| !s.nodes.is_empty()) {
            break status;
        }
        assert!(tokio::time::Instant::now() < deadline, "no per-node status within 180s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    for node in &status.nodes {
        println!("{}: verified={} writer={:?} reason={:?}", node.name, node.verified, node.writer_prefix, node.reason);
    }
    assert!(status.nodes.iter().all(|n| !n.name.is_empty() && !n.address.is_empty()));
    assert_eq!(status.rewrite.total, 0, "this test must never trigger a rewrite");
}
```

Run: `cargo test --test integration_etcd_encryption` — expected: `1 ignored`, no failures.

- [ ] **Step 2: Rewrite the runbook**

`docs/runbooks/etcd-encryption-verification.md` (replace the whole file; layout follows `docs/runbooks/snapshot-controller-verification.md`). It must contain, in this order, each with exact commands and an "Expected:" line:

0. **What this does and does not do.** It observes each control-plane apiserver, optionally rewrites every Secret, and proves per apiserver that nothing is stored under a legacy provider. It does not install the KMS plugin, enable KMS or edit your EncryptionConfiguration. EXPERIMENTAL; not live-verified; not for clusters you care about. State the live findings it was designed from (static-pod plugin, secretbox as a read fallback, the four metrics lines from the spec).
1. **Prerequisites.** The apiserver already uses a KMS provider (`kubectl get --raw '/readyz?verbose' | grep kms-providers` shows `[+]kms-providers ok`); a baseline of provider use (`kubectl get --raw /metrics | grep apiserver_storage_transformation_operations_total | grep secrets`) showing the `to_storage` prefix of your KMS provider; take an etcd snapshot (`talosctl -n <cp> etcd snapshot db.snapshot`).
2. **Migrating from v0.1.11.** `kubectl delete etcdenc default` (the new controller removes the old finalizer; expected: the object disappears), then apply the new example. If you scaled the controller to 0 earlier, scale it back to 2.
3. **Apply and observe.** `kubectl apply -f deploy/crd.yaml`, the controller at the new image, then the example. Interpreting `kubectl get etcdenc default -o yaml`: each phase and what it means, the per-node `status.nodes[]` fields, `status.legacyPrefixes`, and the `Ready` condition message. Common reasons: `cannot verify node X: ...` (see troubleshooting), `NotConfigured`, `mixed`.
4. **Migrate.** Set `rewrite: Enabled` (`kubectl patch etcdenc default --type merge -p '{"spec":{"rewrite":"Enabled"}}'`); expected: `Migrating`, `status.rewrite` counters rising, then `ReadyToRemoveLegacy`. Explain the rewrite re-saves each Secret unchanged and never logs Secret data.
5. **Remove the legacy provider (your change).** Remove secretbox (and any other legacy provider) from your Talos `KubeEtcdEncryptionConfig`, one control-plane node at a time; after each node wait until that node's `kube-apiserver-<node>` pod has a start time after the patch (`kubectl -n kube-system get pod kube-apiserver-<node> -o jsonpath='{.status.startTime}'`) — `/readyz` goes through a load balancer and does not prove the patched node restarted. Then set `acknowledgements.legacyProvidersRemoved: true`. Expected: `Verified`, `Ready=True`. State plainly that the metrics cannot prove the config no longer lists the provider.
6. **Troubleshooting.** `tls: bad certificate` / `certificate is valid for ... not ...`: the apiserver's serving certificate does not accept the server name `kubernetes.default.svc` at a node IP — record the exact error and the certificate's SANs (`openssl s_client -connect <node-ip>:6443 </dev/null 2>/dev/null | openssl x509 -noout -ext subjectAltName`) as a finding; the status will say "cannot verify" and never claim safe. A node `Unverifiable`/timeout: reachability from the controller pod. Counters "went backwards": an apiserver restarted mid-check; it retries.
7. **Findings to record** (the spec's open items): (a) did TLS to `https://<node-ip>:6443` with server name `kubernetes.default.svc` work; (b) is the `kms-providers` readiness line named exactly so; (c) did the limit-paged list read from etcd (the reader check's completeness guards this: note any node reported incomplete); (d) what `transformer_prefix` plaintext/identity reads report (create an unencrypted object only on a throwaway cluster); (e) whether a zero-value counter series is absent. Update `src/transformation_metrics.rs` / `src/apiserver_probe.rs` and their tests if any differ.
8. **Known limitations** (mirror the spec): no plugin install, no Talos patches, no key management; the reader check lists all Secrets on every control-plane node each run (steady state every 10 minutes); the port is fixed at 6443.

- [ ] **Step 3: README, memory, ledger**

`deploy/README.md`: replace the "etcd Secret encryption (optional)" section with a short one stating: `examples/etcd-encryption.yaml` is an `EtcdEncryption` that observes, optionally migrates, and verifies Secret encryption on a cluster whose apiserver already uses a KMS provider; it does **not** install the plugin or touch the apiserver config; EXPERIMENTAL / NOT live-verified; the v0.1.11 schema was replaced (delete the old object first); link to the runbook; note that `rewrite: Enabled` re-saves every Secret and the controller needs `get/list/update` on all Secrets.

`docs/memory/etcd-encryption-2026-09.md`: rewrite the body (keep the frontmatter name, update `description`) to record: the live findings of 2026-10-01 (static-pod plugin sharing `/var/lib/kms/kms.sock`; the DaemonSet's circular dependency on a KMS-encrypted credentials Secret; readyz `[+]kms-providers ok`; secretbox as a read fallback; the four metric lines; `key2:` sub-prefix; the `envelope_encryption_*` family has a bare gauge whenever a provider is configured so it cannot show use); the redesign (stateless derivation, per-apiserver reader and writer checks, no finalizer or ledger, the one-time v0.1.11 finalizer strip); what was deleted and why; the open verification items; link `[[rbac-cluster-admin-tradeoff]]`. Update the `MEMORY.md` index line to match.

`docs/memory/rbac-cluster-admin-tradeoff.md`: update the `EtcdEncryption` ledger row: remove `daemonsets`; keep `get`, `list`, `update` on every Secret in every namespace (the rewrite and the per-apiserver lists); add `list` on `nodes`, `get` on `nonResourceURLs` `/metrics` and `/readyz`; and `create/patch/get/delete` on one Secret (`kube-system/etcd-encryption-canary`). Note that the controller dials each control-plane apiserver directly with its service account token.

- [ ] **Step 4: Verify and commit**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|failed"; grep -rn "keyId\|DaemonSet\|kmsConfigApplied" docs/runbooks/etcd-encryption-verification.md deploy/README.md examples/etcd-encryption.yaml docs/memory/etcd-encryption-2026-09.md | head`
Expected: all suites `ok`; the grep prints nothing except explanatory mentions in the memory file's "what was deleted" section.

```bash
git add tests/integration_etcd_encryption.rs docs/runbooks/etcd-encryption-verification.md deploy/README.md docs/memory
git commit -m "docs: EtcdEncryption adopt-mode runbook, integration test, memory and ledger

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Whole-branch verification

**Files:** none new.

- [ ] **Step 1: Run the CI commands**

Run: `cargo build 2>&1 | tail -2 && cargo test 2>&1 | grep -E "^test result|FAILED|failed" && cargo clippy --all-targets 2>&1 | grep -c "^warning"`
Expected: build OK; every suite `ok` with `0 failed` (ignored cluster tests stay ignored); the warning count is not above the baseline on `main` (the `result_large_err` pattern is shared with the sibling reconcilers).

- [ ] **Step 2: CRD manifest and removed-code checks**

Run: `cargo run -q --bin crdgen | diff - deploy/crd.yaml && echo CRD_FRESH; grep -c uint64 deploy/crd.yaml; ls src | grep -E "kms_|talos_patches|encryption_phase|encryption_probe"`
Expected: `CRD_FRESH`; `0`; and no output from the `ls` (all install-flow modules are gone).

- [ ] **Step 3: Spec/plan reconciliation**

Run: `grep -rn "finalizer\|FINALIZER" src/etcd_encryption_reconciler.rs | head`
Expected: only the one-time strip of `LEGACY_FINALIZER`; no finalizer is added anywhere in this component.

- [ ] **Step 4: Report honestly**

State plainly in the PR description that this slice is **not live-verified**: the open items in the spec (TLS to node IPs with the overridden server name, the `kms-providers` readiness line, the limit-paged list reading from etcd, the identity prefix label, absent zero-value series) are settled only by running `docs/runbooks/etcd-encryption-verification.md` on a real cluster. State that it replaces the v0.1.11 schema (breaking, alpha) and that it fixes the v0.1.11 open issues by removing the machinery they lived in.
