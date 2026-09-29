# SnapshotController Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a sixth platform component to the controller: a `SnapshotController` custom resource that installs the cluster-wide CSI VolumeSnapshot support (the `snapshot.storage.k8s.io`/`groupsnapshot.storage.k8s.io` CRDs and the `snapshot-controller` itself) that `CsiDriver`'s `csi-snapshotter` sidecar needs to actually produce a `VolumeSnapshot`, following the exact shape of `CniInstallation`/`PullThroughCache`/`CloudControllerManager`/`CsiDriver`/`CertManagerInstallation`.

**Architecture:** A new cluster-scoped singleton CRD, `SnapshotController`, with its own reconciler (`src/snapshot_controller_reconciler.rs`) that renders the `snapshot-controller` Helm chart (`piraeusdatastore/helm-charts`, classic repo), synthesizes and applies a plain `snapshot-controller` namespace, applies a self-signed `cert-manager.io/v1` `Issuer` this reconciler owns (not `CertManagerInstallation`, which deliberately configures none), server-side-applies the rendered objects, and prunes and cleans up through the same primitives the other five reconcilers use. `main.rs` runs a sixth `Controller` under the same leader lease. Group-snapshot support and its conversion webhook are in scope (the chart's own default), which is why this component has a hard, same-reconcile dependency on `CertManagerInstallation` already being applied — handled by the existing `wait_for_object_kind` mechanism, not a new one.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, the `helm` CLI (already a runtime dependency, v4.2.4 used to derive the facts below), chart `snapshot-controller` `5.3.0` (app `v8.6.0`) from `https://piraeus.io/helm-charts/`.

**Spec:** [docs/superpowers/specs/2026-09-29-snapshot-controller-design.md](../specs/2026-09-29-snapshot-controller-design.md)

## Global Constraints

Every task's requirements implicitly include this section. Values are copied from the spec, plus facts confirmed live by actually rendering the real chart while writing this plan:

```sh
helm template snapshot-controller --repo https://piraeus.io/helm-charts/ snapshot-controller \
  --version 5.3.0 --include-crds --no-hooks --namespace snapshot-controller \
  --values <(cat <<'EOF'
installCRDs: true
webhook:
  enabled: true
  tls:
    autogenerate: false
    certManagerIssuerRef: { name: snapshot-controller-selfsigned, kind: Issuer }
EOF
)
```

(2026-09-29, helm v4.2.4, exit 0, no stderr.)

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `SnapshotController`, **cluster-scoped**, shortname `snapctl`, plural `snapshotcontrollers` (kube-derive default), singleton named `default`.
- `platformKind` accepts only `talos-linux` (the shared `PlatformKind` enum has one variant).
- **No `provider` field.** Flat spec, same reasoning as `CertManagerInstallation`: `{ platformKind, chartVersion, helmValues }`.
- `chartVersion` is the Helm chart version (`5.3.0`), not the app version (`v8.6.0`) — chart and app versions do **not** track together on this chart, unlike cert-manager. Required, no default, leading/trailing whitespace rejected.
- `helmValues` is optional free-form passthrough, merged first; four typed values are then overlaid so they always win: `installCRDs: true`, `webhook.enabled: true`, `webhook.tls.autogenerate: false`, `webhook.tls.certManagerIssuerRef: {name: snapshot-controller-selfsigned, kind: Issuer}`. Unlike `CertManagerInstallation` (which silently overrides a conflicting `crds.enabled`), a `helmValues` attempt to set any of `installCRDs`, `webhook.enabled`, or anything under `webhook.tls` is **rejected outright** at validation (`InvalidHelmValues`) rather than silently overridden — a webhook TLS misconfiguration fails loudly, not quietly.
- Chart: classic repo `https://piraeus.io/helm-charts/`, chart name `snapshot-controller`, release `snapshot-controller`, namespace `snapshot-controller`. `render_args` already appends `--include-crds --no-hooks --namespace <ns>` for every `ChartRef` — only a new `ChartRef` constant is needed, no new render-argument logic.
- **Live-verified facts about chart `5.3.0` with the values above, used to write test assertions in this plan (not guessed):**
  - `installCRDs: true` renders exactly **6** CRDs, in this document order: `volumesnapshotclasses.snapshot.storage.k8s.io`, `volumesnapshots.snapshot.storage.k8s.io`, `volumesnapshotcontents.snapshot.storage.k8s.io`, `volumegroupsnapshotclasses.groupsnapshot.storage.k8s.io`, `volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io`, `volumegroupsnapshots.groupsnapshot.storage.k8s.io`.
  - The chart renders **no `Namespace`** and **no `Secret`** object (the webhook's TLS Secret is produced later, at runtime, by cert-manager's own `Certificate` controller — not by `helm template`).
  - Exactly **2** `Deployment`s: `snapshot-controller` (the controller) and `snapshot-controller-conversion-webhook`. Exactly **1** `Service` (`snapshot-controller-conversion-webhook` — the controller serves no traffic). **1** `ClusterRole`/`ClusterRoleBinding` pair and **1** `Role`/`RoleBinding` pair (both named `snapshot-controller`, the latter for leader-election `Lease` access). **2** `ServiceAccount`s.
  - Exactly **1** `Certificate` object (`snapshot-controller-conversion-webhook`, in the `snapshot-controller` namespace), with `spec.issuerRef: {name: snapshot-controller-selfsigned, kind: Issuer}` — confirms `webhook.tls.certManagerIssuerRef` flows through to the rendered object exactly as set. No `ValidatingWebhookConfiguration`/`MutatingWebhookConfiguration` object: this chart's "webhook" is a CRD **conversion** webhook, wired via each CRD's own `spec.conversion.webhook` block, not a separate admission-webhook object.
  - The controller `Deployment`'s args include `--feature-gates=CSIVolumeGroupSnapshot=true` — the chart's own default, left unoverridden (group-snapshot support is in scope per the approved design).
  - Zero occurrences of `hostPath` anywhere in the rendered output; `hostNetwork: false` appears (twice, once per Deployment) but never `true`. Both Deployments' containers drop every capability and set `runAsNonRoot: true`, but — unlike cert-manager's chart — set no explicit `seccompProfile`. This is why the synthesized namespace is designed to carry **no** `pod-security.kubernetes.io/*` labels (matching `cert-manager`'s namespace, not Calico's/Spegel's), but it is a **stated assumption**, not a `restricted`-PSS-safe live confirmation the way cert-manager's was — confirm or correct it live in Task 6's runbook.
  - Rendering with `--no-hooks` produces zero `helm.sh/hook` occurrences.
  - This is a classic-repo chart (`ChartSource::Repo`), not OCI, so `strip_oci_pull_preamble` is not relevant here — no `Pulled:`/`Digest:` stdout preamble to worry about, same as Calico/CCM/Cinder.
- The self-signed `Issuer` this reconciler applies is **not** part of the chart's own rendered objects — it is a small, hand-built `DynamicObject`, the exact recipe from the chart's own README (`spec: { selfSigned: {} }`), applied and tracked in this component's own ledger like everything else. This is what resolves the shared-ownership question the queued `csi-snapshot-support-2026-09` research left open: nothing here is jointly owned with any `CsiDriver` or with `CertManagerInstallation`.
- **The hard dependency on `CertManagerInstallation` needs no new error variant or bespoke wait step.** The `Issuer` object is a `cert-manager.io` custom resource; `crate::manifests::is_custom_resource`/`crate::reconciler::wait_for_object_kind` — the exact mechanism `CsiDriver` already uses for `StorageClass`/`CSIDriver` and that `CniInstallation` uses for the tigera operator's own CRDs — already waits (up to `KIND_AVAILABLE_TIMEOUT` = 180s) for a custom resource's kind to be registered before applying it. Calling it explicitly for the hand-built `Issuer`, and relying on the existing per-object `is_custom_resource` check in the main apply loop for the chart's own `Certificate`, covers this with no new code path: if `CertManagerInstallation` isn't applied yet, this surfaces as `Failed`/`ApplyFailed` (the same reason every other `KindNotAvailable` today produces) and retries automatically once it catches up.
- No `cleanupTimeoutSeconds`: cleanup is plain reverse-ledger-order deletion, same as `CertManagerInstallation`.
- **Deleting a `SnapshotController` cascades to delete every `VolumeSnapshot`/`VolumeSnapshotContent`/`VolumeSnapshotClass`/`VolumeGroupSnapshot*` object cluster-wide**, not just ones this component manages — `installCRDs: true` puts the chart's own 6 CRDs in this component's ledger, and Kubernetes deletes every instance of a kind when its CRD is deleted. Same class of hazard as `CertManagerInstallation`'s own cleanup, documented plainly, not coded around.
- The finalizer name `platform.rye.ninja/cleanup` is reused (finalizers are per object).
- **Apply order:** after `CertManagerInstallation` is `Ready` (hard dependency, see above) and after `CniInstallation` is `Ready` (pods run on the pod network and need cluster DNS, same reasoning as `PullThroughCache`/cert-manager). No dependency on `CloudControllerManager` or `CsiDriver`, though installing this is what makes `CsiDriver`'s `csi-snapshotter` sidecar's log noise stop.
- Existing CNI/cache/CCM/CSI/cert-manager reconcile and cleanup logic is not modified. Only `tests/bootstrap_manifests.rs`'s `crd_yaml_defines_all_platform_resources` gains a sixth name.
- CI runs `cargo build`, `cargo test`, `cargo clippy --all-targets`. Clippy warnings are not denied; `result_large_err` already fires on the existing reconcilers and will fire on this one too — accepted.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Building needs free disk (a debug build of this crate is several GB). If a build fails with `No space left on device`, free space first; do not delete anything that is not a build artifact.
- CLAUDE.md: project memory lives in `docs/memory/` (index in `docs/memory/MEMORY.md`), committed like any other change. Do not write to the out-of-repo memory path.

## Review Focus

Failure modes the spec implies but that are easiest to miss. Each has a test in the task that owns the code.

1. A second `SnapshotController` with any name other than `default` must get `Failed`/`Unsupported`, not be reconciled. Test in Task 3.
2. A `helmValues` passthrough that tries to set `installCRDs`, `webhook.enabled`, or anything under `webhook.tls` must be **rejected** at validation (`InvalidHelmValues`) — not silently overridden (unlike `CertManagerInstallation`'s `crds.enabled`) and not left to fail obscurely inside `helm template`. Test in Task 1.
3. Applying `SnapshotController` before `CertManagerInstallation` — the `Issuer`'s `cert-manager.io/v1` kind isn't registered yet — must surface `Failed`/`ApplyFailed` via the existing `KindNotAvailable`/`wait_for_object_kind` path and retry automatically, not hang forever or crash the reconciler with an unrelated error. Test in Task 3 (unit, `ApplyError` → `ApplyFailed` mapping) and noted as a manual check in Task 6's runbook.
4. `helmValues` that is not a JSON object (a string, array, number or `null`) must fail validation with `InvalidHelmValues`, the same as every other component. Test in Task 1.
5. A standby replica, or a validation failure, must not overwrite status with a generic failure — `Validation` already writes its own `Failed` status, and a non-leader must never write status at all. Test in Task 3.

---

## Branch

The spec and this plan are committed on `main`. Create the implementation branch from there:

```bash
git checkout -b snapshot-controller
```

## File Structure

| File | Responsibility |
|---|---|
| `src/snapshot_controller.rs` (create) | `SnapshotController` CRD types, status type, spec validation, the values builder, the `Issuer` name constant. |
| `src/snapshot_controller_reconciler.rs` (create) | Reconcile, cleanup, finalizer wiring, status, and the namespace/`Issuer` object builders. |
| `src/helm.rs` (modify) | `SNAPSHOT_CONTROLLER_NAMESPACE`, `PIRAEUS_CHART_REPO`, `SNAPSHOT_CONTROLLER_CHART`; render-args test; ignored real-chart test. |
| `src/lib.rs` (modify) | Register the two new modules. |
| `src/crds.rs` (modify) | Include the sixth CRD in `generated_yaml()`. |
| `src/main.rs` (modify) | Sixth watcher and `Controller<SnapshotController>`. |
| `deploy/crd.yaml` (regenerate) | All six CRDs. |
| `deploy/README.md` (modify) | Establish-wait for the sixth CRD, apply-order notes, new "Cluster-wide CSI snapshot support" section. |
| `examples/snapshot-controller.yaml` (create) | Talos starting point. |
| `tests/bootstrap_manifests.rs` (modify) | Six CRD names. |
| `tests/snapshot_controller_example.rs` (create) | The example parses, validates and builds the expected values. |
| `tests/integration_snapshot_controller.rs` (create) | Ignored: apply (after `CertManagerInstallation`), assert the two Deployments, delete, assert cleanup including CRD cascade. |
| `docs/runbooks/snapshot-controller-verification.md` (create) | Namespace-admission check, `Issuer`/`Certificate` smoke test, a real `VolumeSnapshot` against a Cinder PVC, the conversion-webhook wiring check, delete/cascade. |
| `docs/memory/snapshot-controller-2026-09.md`, `docs/memory/MEMORY.md` (create/modify) | Memory entry and index line. |
| `docs/memory/csi-snapshot-support-2026-09.md` (modify) | Mark the CRD/controller/webhook half of the queued sub-project as superseded by this slice; the typed-`VolumeSnapshotClass`-on-`CsiDriver` half remains genuinely queued. |
| `docs/memory/rbac-cluster-admin-tradeoff.md` (modify) | New ledger row for the chart's `ClusterRole`/`Role` and the `Issuer`/`Certificate` resources this controller now applies directly. |

---

### Task 1: CRD types, validation and values builder

**Files:**
- Create: `src/snapshot_controller.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` module in `src/snapshot_controller.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`; `crate::pull_through_cache::{merge, preserve_unknown_object}` (both already `pub(crate)`, reused unchanged by three other components today).
- Produces:
  - `SnapshotController` (the CRD kind), `SnapshotControllerSpec { platform_kind: PlatformKind, chart_version: String, helm_values: Option<serde_json::Value> }`, `SnapshotControllerStatus { phase, observed_generation: i64, chart_version: String, applied_resources: Vec<AppliedResourceRef>, conditions: Vec<Condition> }`
  - `SNAPSHOT_CONTROLLER_ISSUER_NAME: &str`
  - `SnapshotControllerSpecError` with `reason(&self) -> &'static str` returning `"InvalidChartVersion"` or `"InvalidHelmValues"`
  - `validate_snapshot_controller(&SnapshotControllerSpec) -> Result<(), SnapshotControllerSpecError>`
  - `build_values(&SnapshotControllerSpec) -> serde_json::Value`

- [ ] **Step 1: Write the failing tests**

Create `src/snapshot_controller.rs` containing only the test module below (the implementation follows in Step 3), and add `pub mod snapshot_controller;` to `src/lib.rs` after `pub mod pull_through_cache;`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn spec() -> SnapshotControllerSpec {
        SnapshotControllerSpec {
            platform_kind: crate::crd::PlatformKind::TalosLinux,
            chart_version: "5.3.0".to_string(),
            helm_values: None,
        }
    }

    #[test]
    fn spec_deserializes_with_only_the_required_fields() {
        let spec: SnapshotControllerSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "chartVersion": "5.3.0"
        }))
        .expect("minimal spec should deserialize");

        assert_eq!(spec.platform_kind, crate::crd::PlatformKind::TalosLinux);
        assert_eq!(spec.chart_version, "5.3.0");
        assert!(spec.helm_values.is_none());
    }

    #[test]
    fn a_spec_without_a_chart_version_is_rejected_at_deserialization() {
        let result = serde_json::from_value::<SnapshotControllerSpec>(serde_json::json!({
            "platformKind": "talos-linux"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn accepts_a_minimal_spec() {
        assert_eq!(validate_snapshot_controller(&spec()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_chart_version() {
        let mut s = spec();
        s.chart_version = "  ".to_string();

        let err = validate_snapshot_controller(&s).expect_err("blank chartVersion is invalid");

        assert_eq!(err, SnapshotControllerSpecError::EmptyChartVersion);
        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn rejects_chart_versions_with_leading_or_trailing_whitespace() {
        for version in [" 5.3.0", "5.3.0 ", "5.3.0\n"] {
            let mut s = spec();
            s.chart_version = version.to_string();

            let err = validate_snapshot_controller(&s).expect_err("whitespace must be rejected");

            assert_eq!(err, SnapshotControllerSpecError::ChartVersionHasWhitespace(version.to_string()));
            assert_eq!(err.reason(), "InvalidChartVersion");
        }
    }

    #[test]
    fn rejects_helm_values_that_are_not_an_object() {
        for value in [serde_json::json!("nope"), serde_json::json!(["a"]), serde_json::json!(3), serde_json::json!(null)] {
            let mut s = spec();
            s.helm_values = Some(value.clone());

            let err = validate_snapshot_controller(&s).expect_err("non-object helmValues is invalid");

            assert_eq!(err, SnapshotControllerSpecError::HelmValuesNotObject, "{value}");
            assert_eq!(err.reason(), "InvalidHelmValues");
        }
    }

    #[test]
    fn accepts_helm_values_that_are_objects() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "controller": { "replicaCount": 2 } }));

        assert_eq!(validate_snapshot_controller(&s), Ok(()));
    }

    #[test]
    fn rejects_helm_values_that_set_install_crds() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "installCRDs": false }));

        let err = validate_snapshot_controller(&s).expect_err("installCRDs via helmValues is invalid");

        assert_eq!(err, SnapshotControllerSpecError::HelmValuesSetInstallCrds);
        assert_eq!(err.reason(), "InvalidHelmValues");
    }

    #[test]
    fn rejects_helm_values_that_set_webhook_enabled() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "webhook": { "enabled": false } }));

        let err = validate_snapshot_controller(&s).expect_err("webhook.enabled via helmValues is invalid");

        assert_eq!(err, SnapshotControllerSpecError::HelmValuesSetWebhookEnabled);
        assert_eq!(err.reason(), "InvalidHelmValues");
    }

    #[test]
    fn rejects_helm_values_that_set_webhook_tls() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "webhook": { "tls": { "autogenerate": true } } }));

        let err = validate_snapshot_controller(&s).expect_err("webhook.tls via helmValues is invalid");

        assert_eq!(err, SnapshotControllerSpecError::HelmValuesSetWebhookTls);
        assert_eq!(err.reason(), "InvalidHelmValues");
    }

    #[test]
    fn accepts_helm_values_that_touch_other_webhook_keys() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "webhook": { "replicaCount": 2 } }));

        assert_eq!(validate_snapshot_controller(&s), Ok(()));
    }

    #[test]
    fn values_always_force_install_crds_webhook_and_the_selfsigned_issuer_ref() {
        let values = build_values(&spec());

        assert_eq!(values["installCRDs"], true);
        assert_eq!(values["webhook"]["enabled"], true);
        assert_eq!(values["webhook"]["tls"]["autogenerate"], false);
        assert_eq!(values["webhook"]["tls"]["certManagerIssuerRef"]["name"], SNAPSHOT_CONTROLLER_ISSUER_NAME);
        assert_eq!(values["webhook"]["tls"]["certManagerIssuerRef"]["kind"], "Issuer");
    }

    #[test]
    fn helm_values_pass_through_alongside_the_typed_values() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "controller": { "replicaCount": 2 }, "webhook": { "replicaCount": 2 } }));

        let values = build_values(&s);

        assert_eq!(values["controller"]["replicaCount"], 2);
        assert_eq!(values["webhook"]["replicaCount"], 2);
        assert_eq!(values["installCRDs"], true);
        assert_eq!(values["webhook"]["enabled"], true);
    }

    #[test]
    fn crd_is_cluster_scoped_with_the_snapctl_shortname() {
        let crd = SnapshotController::crd();

        assert_eq!(crd.metadata.name.as_deref(), Some("snapshotcontrollers.platform.rye.ninja"));
        assert_eq!(crd.spec.group, "platform.rye.ninja");
        assert_eq!(crd.spec.names.kind, "SnapshotController");
        assert_eq!(crd.spec.scope, "Cluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["snapctl".to_string()]));
    }

    #[test]
    fn crd_schema_lets_helm_values_carry_arbitrary_keys() {
        let yaml = serde_yaml::to_string(&SnapshotController::crd()).expect("CRD serializes");

        assert!(
            yaml.contains("x-kubernetes-preserve-unknown-fields: true"),
            "helmValues must be a structural, unknown-field-preserving object:\n{yaml}"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib snapshot_controller:: 2>&1 | tail -40`
Expected: FAIL to compile — `SnapshotControllerSpec`, `validate_snapshot_controller`, `build_values`, `SnapshotControllerSpecError`, `SnapshotController`, `SNAPSHOT_CONTROLLER_ISSUER_NAME` are not defined yet.

- [ ] **Step 3: Write the implementation**

Add above the test module in `src/snapshot_controller.rs`:

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Name of the self-signed cert-manager `Issuer` the reconciler creates for
/// the conversion webhook's TLS. Shared between `build_values` (which points
/// `webhook.tls.certManagerIssuerRef` at it) and
/// `snapshot_controller_reconciler::snapshot_controller_selfsigned_issuer_object`
/// (which builds the actual `Issuer` object).
pub const SNAPSHOT_CONTROLLER_ISSUER_NAME: &str = "snapshot-controller-selfsigned";

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "SnapshotController",
    status = "SnapshotControllerStatus",
    shortname = "snapctl"
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotControllerSpec {
    pub platform_kind: PlatformKind,
    pub chart_version: String,
    /// Free-form values merged into the chart's values. The controller's own
    /// typed values (`installCRDs`, `webhook.enabled`, `webhook.tls.*`) are
    /// overlaid afterwards, so they always win -- and `validate_snapshot_controller`
    /// rejects a `helmValues` attempt to set any of them outright, rather than
    /// silently overriding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::pull_through_cache::preserve_unknown_object")]
    pub helm_values: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotControllerStatus {
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

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum SnapshotControllerSpecError {
    #[error("spec.chartVersion must not be empty")]
    EmptyChartVersion,
    #[error("spec.chartVersion {0:?} has leading or trailing whitespace")]
    ChartVersionHasWhitespace(String),
    #[error("spec.helmValues must be a JSON object")]
    HelmValuesNotObject,
    #[error(
        "spec.helmValues must not set installCRDs: it is always forced true so the cluster-wide \
         snapshot CRDs are never silently skipped"
    )]
    HelmValuesSetInstallCrds,
    #[error(
        "spec.helmValues must not set webhook.enabled: the conversion webhook is always forced \
         on for group-snapshot support"
    )]
    HelmValuesSetWebhookEnabled,
    #[error(
        "spec.helmValues must not set webhook.tls.*: TLS is always wired to the self-signed \
         cert-manager Issuer this component creates"
    )]
    HelmValuesSetWebhookTls,
}

impl SnapshotControllerSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            SnapshotControllerSpecError::EmptyChartVersion
            | SnapshotControllerSpecError::ChartVersionHasWhitespace(_) => "InvalidChartVersion",
            SnapshotControllerSpecError::HelmValuesNotObject
            | SnapshotControllerSpecError::HelmValuesSetInstallCrds
            | SnapshotControllerSpecError::HelmValuesSetWebhookEnabled
            | SnapshotControllerSpecError::HelmValuesSetWebhookTls => "InvalidHelmValues",
        }
    }
}

/// Rejects specs the chart would silently mis-handle, before anything is
/// applied.
pub fn validate_snapshot_controller(
    spec: &SnapshotControllerSpec,
) -> Result<(), SnapshotControllerSpecError> {
    if spec.chart_version.trim().is_empty() {
        return Err(SnapshotControllerSpecError::EmptyChartVersion);
    }
    if spec.chart_version.trim() != spec.chart_version {
        return Err(SnapshotControllerSpecError::ChartVersionHasWhitespace(spec.chart_version.clone()));
    }
    let Some(values) = &spec.helm_values else {
        return Ok(());
    };
    if !values.is_object() {
        return Err(SnapshotControllerSpecError::HelmValuesNotObject);
    }
    if values.get("installCRDs").is_some() {
        return Err(SnapshotControllerSpecError::HelmValuesSetInstallCrds);
    }
    if values.get("webhook").and_then(|webhook| webhook.get("enabled")).is_some() {
        return Err(SnapshotControllerSpecError::HelmValuesSetWebhookEnabled);
    }
    if values.get("webhook").and_then(|webhook| webhook.get("tls")).is_some() {
        return Err(SnapshotControllerSpecError::HelmValuesSetWebhookTls);
    }
    Ok(())
}

/// The Helm values for the snapshot-controller chart: the user's
/// `helmValues` passthrough, with the controller's typed values overlaid on
/// top so they always win.
///
/// `installCRDs: true` is also this chart's own default (live-verified
/// against 5.3.0), but forced here the same way `CertManagerInstallation`
/// forces `crds.enabled` -- defense against a future chart-default flip, not
/// defensive redundancy today. `webhook.enabled: true` turns on the
/// conversion webhook the group-snapshot CRDs need; `webhook.tls.autogenerate:
/// false` plus `certManagerIssuerRef` point its TLS at the self-signed
/// `Issuer` the reconciler creates (`SNAPSHOT_CONTROLLER_ISSUER_NAME`),
/// instead of the chart's own Helm-generated self-signed cert -- the exact
/// recipe from the chart's own README.
pub fn build_values(spec: &SnapshotControllerSpec) -> serde_json::Value {
    let mut values = spec.helm_values.clone().unwrap_or_else(|| serde_json::json!({}));

    let typed = serde_json::json!({
        "installCRDs": true,
        "webhook": {
            "enabled": true,
            "tls": {
                "autogenerate": false,
                "certManagerIssuerRef": {
                    "name": SNAPSHOT_CONTROLLER_ISSUER_NAME,
                    "kind": "Issuer",
                },
            },
        },
    });

    crate::pull_through_cache::merge(&mut values, typed);
    values
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib snapshot_controller:: 2>&1 | tail -40`
Expected: PASS, 15 tests.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot_controller.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add the SnapshotController CRD type and values builder

New sixth platform component: CRD types, status, spec validation and
the Helm values builder for installing the cluster-wide CSI
VolumeSnapshot CRDs and controller. Unlike CertManagerInstallation, a
helmValues attempt to set installCRDs/webhook.enabled/webhook.tls.* is
rejected outright rather than silently overridden.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Helm chart wiring

**Files:**
- Modify: `src/helm.rs`
- Test: inline `#[cfg(test)]` module in `src/helm.rs`

**Interfaces:**
- Consumes: `ChartRef`, `ChartSource`, `render_args`, `render_chart` (all already defined in `src/helm.rs`); `crate::snapshot_controller::{SnapshotControllerSpec, build_values, SNAPSHOT_CONTROLLER_ISSUER_NAME}` from Task 1.
- Produces: `SNAPSHOT_CONTROLLER_NAMESPACE: &str`, `PIRAEUS_CHART_REPO: &str`, `SNAPSHOT_CONTROLLER_CHART: ChartRef`.

- [ ] **Step 1: Write the failing test**

In `src/helm.rs`'s `#[cfg(test)] mod tests` block, add (near the other `render_args` tests):

```rust
#[test]
fn snapshot_controller_render_args_use_the_piraeus_repo_and_namespace() {
    let path = std::path::Path::new("/tmp/values.yaml");
    let args = render_args(&SNAPSHOT_CONTROLLER_CHART, "5.3.0", path);

    assert_eq!(
        args,
        vec![
            "template".to_string(),
            "snapshot-controller".to_string(),
            "--repo".to_string(),
            "https://piraeus.io/helm-charts/".to_string(),
            "snapshot-controller".to_string(),
            "--version".to_string(),
            "5.3.0".to_string(),
            "--values".to_string(),
            "/tmp/values.yaml".to_string(),
            "--include-crds".to_string(),
            "--no-hooks".to_string(),
            "--namespace".to_string(),
            "snapshot-controller".to_string(),
        ]
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib helm::tests::snapshot_controller_render_args -- --exact 2>&1 | tail -20`
Expected: FAIL to compile — `SNAPSHOT_CONTROLLER_CHART` is not defined yet.

- [ ] **Step 3: Add the chart constants**

In `src/helm.rs`, add after the `CERT_MANAGER_NAMESPACE` constant:

```rust
/// Namespace the snapshot-controller chart's namespaced objects belong in.
/// Like tigera-operator, Spegel and cert-manager, the chart renders no
/// `Namespace` object of its own (live-verified against chart 5.3.0).
pub const SNAPSHOT_CONTROLLER_NAMESPACE: &str = "snapshot-controller";

/// The Helm repository the snapshot-controller chart is fetched from --
/// piraeusdatastore/helm-charts' classic repo, sourced directly from
/// kubernetes-csi/external-snapshotter (there is no official chart from
/// kubernetes-csi itself).
pub const PIRAEUS_CHART_REPO: &str = "https://piraeus.io/helm-charts/";
```

Add after the `CERT_MANAGER_CHART` constant:

```rust
pub const SNAPSHOT_CONTROLLER_CHART: ChartRef = ChartRef {
    release: "snapshot-controller",
    source: ChartSource::Repo {
        url: PIRAEUS_CHART_REPO,
        chart: "snapshot-controller",
    },
    namespace: SNAPSHOT_CONTROLLER_NAMESPACE,
};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib helm::tests::snapshot_controller_render_args -- --exact 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Add the ignored real-chart test**

In `src/helm.rs`'s test module, add near the other ignored real-chart tests:

```rust
#[tokio::test]
#[ignore = "requires network access and the helm CLI to be installed"]
async fn snapshot_controller_chart_renders_the_shape_the_spec_relies_on() {
    let spec = crate::snapshot_controller::SnapshotControllerSpec {
        platform_kind: crate::crd::PlatformKind::TalosLinux,
        chart_version: "5.3.0".to_string(),
        helm_values: None,
    };
    let values = crate::snapshot_controller::build_values(&spec);

    let rendered = render_chart(&SNAPSHOT_CONTROLLER_CHART, &spec.chart_version, &values)
        .await
        .expect("helm template should succeed");
    let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

    let kinds: Vec<&str> = objects
        .iter()
        .map(|o| o.types.as_ref().expect("every rendered object has a type").kind.as_str())
        .collect();

    // installCRDs: true is always set (build_values), so all six CRDs
    // render, in the chart's own document order (live-verified against 5.3.0).
    let crd_names: Vec<&str> = objects
        .iter()
        .filter(|o| o.types.as_ref().unwrap().kind == "CustomResourceDefinition")
        .map(|o| o.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        crd_names,
        vec![
            "volumesnapshotclasses.snapshot.storage.k8s.io",
            "volumesnapshots.snapshot.storage.k8s.io",
            "volumesnapshotcontents.snapshot.storage.k8s.io",
            "volumegroupsnapshotclasses.groupsnapshot.storage.k8s.io",
            "volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io",
            "volumegroupsnapshots.groupsnapshot.storage.k8s.io",
        ],
        "{crd_names:?}"
    );
    assert!(!kinds.contains(&"Namespace"), "{kinds:?}");
    assert!(!kinds.contains(&"Secret"), "{kinds:?}");

    // Exactly the controller and conversion-webhook Deployments.
    let mut deployment_names: Vec<&str> = objects
        .iter()
        .filter(|o| o.types.as_ref().unwrap().kind == "Deployment")
        .map(|o| o.metadata.name.as_deref().unwrap())
        .collect();
    deployment_names.sort_unstable();
    assert_eq!(
        deployment_names,
        vec!["snapshot-controller", "snapshot-controller-conversion-webhook"],
        "{deployment_names:?}"
    );

    // Exactly one Service, for the webhook -- the controller serves no traffic.
    let service_names: Vec<&str> = objects
        .iter()
        .filter(|o| o.types.as_ref().unwrap().kind == "Service")
        .map(|o| o.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(service_names, vec!["snapshot-controller-conversion-webhook"], "{service_names:?}");

    // The webhook's Certificate references the self-signed Issuer this
    // component creates, by name and kind, not the chart's own
    // Helm-generated cert (webhook.tls.autogenerate: false, verified here).
    let certificate = objects
        .iter()
        .find(|o| o.types.as_ref().unwrap().kind == "Certificate")
        .expect("chart renders a Certificate when webhook.enabled and webhook.tls.certManagerIssuerRef are set");
    assert_eq!(
        certificate.data["spec"]["issuerRef"]["name"],
        crate::snapshot_controller::SNAPSHOT_CONTROLLER_ISSUER_NAME
    );
    assert_eq!(certificate.data["spec"]["issuerRef"]["kind"], "Issuer");

    // Group-snapshot support is on (the chart's own default, left
    // unoverridden per the approved design).
    assert!(rendered.contains("--feature-gates=CSIVolumeGroupSnapshot=true"), "{rendered}");

    // No hostPath anywhere, and hostNetwork explicitly false (never true) on
    // both Deployments -- supports (but does not by itself confirm) the
    // synthesized namespace needing no pod-security.kubernetes.io/* labels;
    // reconfirm live per Task 6's runbook.
    assert!(!rendered.contains("hostPath"), "{rendered}");
    assert!(!rendered.contains("hostNetwork: true"), "{rendered}");

    // --no-hooks: no hook objects are rendered as live objects.
    assert!(!rendered.contains("helm.sh/hook"));
}
```

- [ ] **Step 6: Run every test to verify nothing else broke**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS (the new `#[ignore]`d test is skipped by default; everything else, including all pre-existing tests, still passes).

- [ ] **Step 7: Commit**

```bash
git add src/helm.rs
git commit -m "$(cat <<'EOF'
feat: wire the snapshot-controller chart into the Helm render layer

Adds SNAPSHOT_CONTROLLER_NAMESPACE/PIRAEUS_CHART_REPO/
SNAPSHOT_CONTROLLER_CHART (classic-repo form) and an ignored real-chart
test that pins the exact rendered shape confirmed live against 5.3.0:
6 CRDs, no Namespace/Secret, exactly 2 Deployments and 1 Service, the
Certificate's issuerRef pointing at the self-signed Issuer, and the
group-snapshot feature gate left on.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Reconciler

**Files:**
- Create: `src/snapshot_controller_reconciler.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` module in `src/snapshot_controller_reconciler.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`; `crate::snapshot_controller::{SnapshotController, SnapshotControllerSpec, SnapshotControllerStatus, SnapshotControllerSpecError, build_values, SNAPSHOT_CONTROLLER_ISSUER_NAME}` (Task 1); `crate::helm::SNAPSHOT_CONTROLLER_CHART` (Task 2); `crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME}` (all already `pub`, `wait_for_object_kind` already reused unchanged by `csi_reconciler.rs`, `ccm_reconciler.rs` and `cache_reconciler.rs` today); `crate::manifests::is_custom_resource` (already `pub`, reused unchanged by `csi_reconciler.rs`).
- Produces:
  - `ValidationError` with `reason(&self) -> &'static str`
  - `validate(name: &str, spec: &SnapshotControllerSpec) -> Result<(), ValidationError>`
  - `snapshot_controller_namespace_object() -> kube::api::DynamicObject`
  - `snapshot_controller_selfsigned_issuer_object() -> kube::api::DynamicObject`
  - `SnapshotControllerReconcileError` with `failure_reason(&self) -> Option<&'static str>`
  - `reconcile(obj: Arc<SnapshotController>, ctx: Arc<Context>) -> Result<Action, SnapshotControllerReconcileError>`
  - `cleanup(obj: Arc<SnapshotController>, ctx: Arc<Context>) -> Result<Action, SnapshotControllerReconcileError>`
  - `error_policy(...) -> Action`
  - `reconcile_with_finalizer(obj: Arc<SnapshotController>, ctx: Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<SnapshotControllerReconcileError>>`

Note: `snapshot_controller_namespace_object` carries **no** `pod-security.kubernetes.io/*` labels, matching `cert_manager_namespace_object` and unlike Calico's/Spegel's namespaces — see Global Constraints for why this is a stated assumption, not yet a live-confirmed one the way cert-manager's is.

- [ ] **Step 1: Write the failing tests**

Create `src/snapshot_controller_reconciler.rs` containing only the code below (implementation follows in Step 3), and add `pub mod snapshot_controller_reconciler;` to `src/lib.rs` after `pub mod snapshot_controller;`.

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME};
use crate::snapshot_controller::{
    SnapshotController, SnapshotControllerSpec, SnapshotControllerSpecError, SnapshotControllerStatus,
    SNAPSHOT_CONTROLLER_ISSUER_NAME,
};
use kube::api::DynamicObject;
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(platform_kind: PlatformKind) -> SnapshotControllerSpec {
        SnapshotControllerSpec {
            platform_kind,
            chart_version: "5.3.0".to_string(),
            helm_values: None,
        }
    }

    #[test]
    fn accepts_talos_linux_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let err = validate("second", &spec_with(PlatformKind::TalosLinux))
            .expect_err("non-singleton names should be rejected");

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.chart_version = String::new();

        let err = validate("default", &spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn synthesized_namespace_is_named_snapshot_controller_with_no_privileged_labels() {
        let object = snapshot_controller_namespace_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("snapshot-controller"));
        assert!(object.metadata.namespace.is_none());
        assert!(object.metadata.labels.is_none());
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&snapshot_controller_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "snapshot-controller");
        assert_eq!(reference.namespace, "");
    }

    #[test]
    fn selfsigned_issuer_object_is_named_and_namespaced_correctly() {
        let object = snapshot_controller_selfsigned_issuer_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "cert-manager.io/v1");
        assert_eq!(types.kind, "Issuer");
        assert_eq!(object.metadata.name.as_deref(), Some(SNAPSHOT_CONTROLLER_ISSUER_NAME));
        assert_eq!(object.metadata.namespace.as_deref(), Some("snapshot-controller"));
        assert_eq!(object.data["spec"]["selfSigned"], serde_json::json!({}));
    }

    #[test]
    fn selfsigned_issuer_object_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&snapshot_controller_selfsigned_issuer_object());

        assert_eq!(reference.api_version, "cert-manager.io/v1");
        assert_eq!(reference.kind, "Issuer");
        assert_eq!(reference.name, SNAPSHOT_CONTROLLER_ISSUER_NAME);
        assert_eq!(reference.namespace, "snapshot-controller");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = SnapshotControllerReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = SnapshotControllerReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_including_a_not_yet_registered_issuer_kind_report_apply_failed() {
        // The exact shape wait_for_object_kind's timeout produces when
        // CertManagerInstallation hasn't been applied yet: this must surface
        // as a retried Failed/ApplyFailed, not hang or panic.
        let err = SnapshotControllerReconcileError::Apply(crate::apply::ApplyError::KindNotAvailable {
            api_version: "cert-manager.io/v1".to_string(),
            kind: "Issuer".to_string(),
            timeout: Duration::from_secs(180),
            detail: "kind not registered yet".to_string(),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        let validation = SnapshotControllerReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(SnapshotControllerReconcileError::NotLeader.failure_reason(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib snapshot_controller_reconciler:: 2>&1 | tail -40`
Expected: FAIL to compile — `validate`, `snapshot_controller_namespace_object`, `snapshot_controller_selfsigned_issuer_object`, `ValidationError`, `SnapshotControllerReconcileError` are not defined yet.

- [ ] **Step 3: Write the implementation**

Insert above the test module in `src/snapshot_controller_reconciler.rs`:

```rust
#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "SnapshotController {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] SnapshotControllerSpecError),
}

impl ValidationError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            ValidationError::UnsupportedPlatform(_) | ValidationError::UnsupportedName(_) => {
                "Unsupported"
            }
        }
    }
}

pub fn validate(name: &str, spec: &SnapshotControllerSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::snapshot_controller::validate_snapshot_controller(spec)?;
    Ok(())
}

/// The chart renders no `Namespace` object, so the controller synthesizes
/// one and applies it ahead of everything else, the same as
/// `cert_manager_namespace_object`. No `pod-security.kubernetes.io/*`
/// labels: the chart's controller and webhook containers drop every
/// capability and run non-root (live-verified rendering chart 5.3.0),
/// though unlike cert-manager's chart they set no explicit `seccompProfile`
/// -- a stated assumption, confirmed or corrected live per the runbook.
pub fn snapshot_controller_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::SNAPSHOT_CONTROLLER_NAMESPACE,
        },
    }))
    .expect("static Namespace JSON deserializes into a DynamicObject")
}

/// A self-signed cert-manager `Issuer`, the exact recipe from the
/// snapshot-controller chart's own README, that the webhook's `Certificate`
/// (rendered by the chart via `webhook.tls.certManagerIssuerRef`) references
/// by name. Applied and tracked by this reconciler like any other object --
/// nothing here is jointly owned with `CertManagerInstallation`, which
/// installs cert-manager itself but deliberately configures no Issuer of its
/// own.
pub fn snapshot_controller_selfsigned_issuer_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "cert-manager.io/v1",
        "kind": "Issuer",
        "metadata": {
            "name": SNAPSHOT_CONTROLLER_ISSUER_NAME,
            "namespace": crate::helm::SNAPSHOT_CONTROLLER_NAMESPACE,
        },
        "spec": {
            "selfSigned": {},
        },
    }))
    .expect("static Issuer JSON deserializes into a DynamicObject")
}

#[derive(thiserror::Error, Debug)]
pub enum SnapshotControllerReconcileError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Helm(#[from] crate::helm::HelmError),
    #[error(transparent)]
    Manifest(#[from] crate::manifests::ManifestError),
    #[error(transparent)]
    Apply(#[from] crate::apply::ApplyError),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
    #[error("not the leader; standing down")]
    NotLeader,
}

impl SnapshotControllerReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// `Apply` covers `ApplyError::KindNotAvailable` too -- the exact error
    /// `wait_for_object_kind` produces when `CertManagerInstallation` (and
    /// therefore the `Issuer`/`Certificate` CRDs) hasn't been applied yet --
    /// so that case surfaces as a retried `Failed`/`ApplyFailed`, with no
    /// new variant needed.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            SnapshotControllerReconcileError::Helm(_) => Some("RenderFailed"),
            SnapshotControllerReconcileError::Manifest(_) => Some("InvalidManifest"),
            SnapshotControllerReconcileError::Apply(_) => Some("ApplyFailed"),
            SnapshotControllerReconcileError::Validation(_)
            | SnapshotControllerReconcileError::Status(_)
            | SnapshotControllerReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status`
/// (with a reason and the ledger of everything that may exist) before the
/// original error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
) -> Result<Action, SnapshotControllerReconcileError> {
    let mut progress = crate::ledger::ReconcileProgress::default();
    let result = reconcile_inner(obj.clone(), ctx.clone(), &mut progress).await;
    if let Err(err) = &result
        && let Some(reason) = err.failure_reason()
    {
        record_failure(&obj, &ctx, &progress, reason, &err.to_string()).await;
    }
    result
}

/// Best effort: a failed status write is logged and never replaces the
/// reconcile's own error.
async fn record_failure(
    obj: &SnapshotController,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<SnapshotController> = kube::Api::all(ctx.client.clone());
    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();
    let ledger = crate::ledger::failure_ledger(&previous, progress.desired.as_deref());

    if let Err(err) = update_status(
        &api,
        &name,
        Phase::Failed,
        obj.metadata.generation,
        &obj.spec.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(installation = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, SnapshotControllerReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<SnapshotController> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(installation = %name, error = %err, "validation failed");
        update_status(
            &api,
            &name,
            Phase::Failed,
            obj.metadata.generation,
            &chart_version,
            &previous,
            err.reason(),
            &err.to_string(),
        )
        .await?;
        return Err(SnapshotControllerReconcileError::Validation(err));
    }
    tracing::info!(
        installation = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::snapshot_controller::build_values(&obj.spec);
    let rendered =
        crate::helm::render_chart(&crate::helm::SNAPSHOT_CONTROLLER_CHART, &chart_version, &values)
            .await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::SNAPSHOT_CONTROLLER_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered snapshot-controller chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    // Everything this reconcile will apply is known now, in apply order: the
    // target namespace, the self-signed Issuer the webhook's Certificate
    // references, then the chart's own rendered objects. Persist it before
    // the first apply so a failure, a crash or a leader change can never
    // leave an applied object out of the ledger cleanup acts on. Steady-state
    // resyncs add nothing, so they write nothing.
    let mut desired = vec![
        crate::apply::resource_ref(&snapshot_controller_namespace_object()),
        crate::apply::resource_ref(&snapshot_controller_selfsigned_issuer_object()),
    ];
    desired.extend(objects.iter().map(crate::apply::resource_ref));
    progress.desired = Some(desired.clone());
    if let Some(ledger) = crate::ledger::checkpoint_ledger(&previous, &desired) {
        tracing::info!(entries = ledger.len(), "checkpointing ledger before applying");
        update_status(
            &api,
            &name,
            Phase::Installing,
            obj.metadata.generation,
            &chart_version,
            &ledger,
            "Applying",
            "applying manifests",
        )
        .await?;
    }

    let mut applied = Vec::new();

    // The chart has no Namespace object of its own; create the target
    // namespace before anything that lives inside it.
    let namespace_ref = crate::apply::apply_object(
        &ctx.client,
        &snapshot_controller_namespace_object(),
        "platform-controller",
    )
    .await?;
    applied.push(namespace_ref);

    // The Issuer is a cert-manager.io custom resource: its CRD only exists
    // once CertManagerInstallation has been applied and reconciled. Waiting
    // here, rather than assuming it, is what turns "CertManagerInstallation
    // isn't applied yet" into a retried Failed/ApplyFailed status instead of
    // a hard, unretried discovery error.
    let issuer_object = snapshot_controller_selfsigned_issuer_object();
    wait_for_object_kind(&ctx.client, &issuer_object).await?;
    let issuer_ref =
        crate::apply::apply_object(&ctx.client, &issuer_object, "platform-controller").await?;
    applied.push(issuer_ref);

    for object in &objects {
        // The chart's own Certificate is a cert-manager.io custom resource
        // too and takes the same wait; every built-in kind this chart
        // renders (CRDs, RBAC, Service, Deployments) has an entry in
        // rank_for_kind's table, so is_custom_resource is only ever true for
        // Certificate here -- same pattern csi_reconciler.rs already uses
        // for StorageClass/CSIDriver.
        if crate::manifests::is_custom_resource(object) {
            wait_for_object_kind(&ctx.client, object).await?;
        }
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        applied.push(reference);
    }
    tracing::info!(applied_count = applied.len(), "applied all objects");

    let stale = crate::apply::resources_to_prune(&previous, &applied);
    let pruned_count = stale.len();
    for reference in stale {
        crate::apply::delete_object(&ctx.client, &reference).await?;
    }
    if pruned_count > 0 {
        tracing::info!(pruned_count, "pruned resources no longer rendered");
    }

    debug_assert!(
        applied.iter().all(|reference| desired.contains(reference)),
        "applied an object that was not in the checkpointed ledger"
    );
    tracing::info!(installation = %name, phase = ?Phase::Ready, "updating status");
    update_status(
        &api,
        &name,
        Phase::Ready,
        obj.metadata.generation,
        &chart_version,
        &applied,
        "Applied",
        "manifests applied successfully",
    )
    .await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy(
    _obj: Arc<SnapshotController>,
    _err: &kube::runtime::finalizer::Error<SnapshotControllerReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<SnapshotController>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), SnapshotControllerReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = SnapshotControllerStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(SnapshotControllerReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
) -> Result<Action, SnapshotControllerReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(SnapshotControllerReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(installation = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order: the namespace and Issuer were applied first, so
    // they go last. installCRDs: true means the chart's own CRDs are in this
    // ledger, so deleting them cascades away every VolumeSnapshot/
    // VolumeGroupSnapshot-family object in the cluster, not just this
    // component's own -- documented, not coded around, same as
    // CertManagerInstallation's cleanup.
    for reference in applied.iter().rev() {
        tracing::info!(
            installation = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(installation = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<SnapshotControllerReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all;
    // see `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<SnapshotController> = kube::Api::all(ctx.client.clone());
    kube::runtime::finalizer(&api, FINALIZER_NAME, obj, |event| async move {
        match event {
            kube::runtime::finalizer::Event::Apply(obj) => reconcile(obj, ctx).await,
            kube::runtime::finalizer::Event::Cleanup(obj) => cleanup(obj, ctx).await,
        }
    })
    .await
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib snapshot_controller_reconciler:: 2>&1 | tail -40`
Expected: PASS, 11 tests.

- [ ] **Step 5: Run the whole library test suite**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS, no regressions.

- [ ] **Step 6: Commit**

```bash
git add src/snapshot_controller_reconciler.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add the SnapshotController reconciler

Same shape as cert_manager_reconciler.rs, plus a hand-built self-signed
cert-manager Issuer applied and tracked in this component's own ledger
(not shared with CertManagerInstallation, which configures none). The
Issuer and the chart's own Certificate both go through the existing
is_custom_resource/wait_for_object_kind path already used for
StorageClass/CSIDriver, so a SnapshotController applied before
CertManagerInstallation surfaces a retried Failed/ApplyFailed rather
than a hard error.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Register the CRD and controller

**Files:**
- Modify: `src/crds.rs`, `src/main.rs`, `tests/bootstrap_manifests.rs`
- Regenerate: `deploy/crd.yaml`

**Interfaces:**
- Consumes: `crate::snapshot_controller::SnapshotController` (Task 1), `crate::snapshot_controller_reconciler::{reconcile_with_finalizer, error_policy}` (Task 3).
- Produces: nothing new — this task only wires existing pieces together.

- [ ] **Step 1: Add the CRD to `generated_yaml`**

In `src/crds.rs`, add `crate::snapshot_controller::SnapshotController::crd(),` as the sixth entry:

```rust
pub fn generated_yaml() -> String {
    [
        crate::crd::CniInstallation::crd(),
        crate::pull_through_cache::PullThroughCache::crd(),
        crate::cloud_controller_manager::CloudControllerManager::crd(),
        crate::csi_driver::CsiDriver::crd(),
        crate::cert_manager::CertManagerInstallation::crd(),
        crate::snapshot_controller::SnapshotController::crd(),
    ]
    .iter()
    .map(|crd| serde_yaml::to_string(crd).expect("CRD should serialize to YAML"))
    .collect::<Vec<_>>()
    .join("---\n")
}
```

- [ ] **Step 2: Update the CRD-count test's expected names**

In `tests/bootstrap_manifests.rs`, change `crd_yaml_defines_all_platform_resources`'s expected `names` to add the sixth entry, in the same order as `generated_yaml`:

```rust
    assert_eq!(
        names,
        vec![
            "cniinstallations.platform.rye.ninja",
            "pullthroughcaches.platform.rye.ninja",
            "cloudcontrollermanagers.platform.rye.ninja",
            "csidrivers.platform.rye.ninja",
            "certmanagerinstallations.platform.rye.ninja",
            "snapshotcontrollers.platform.rye.ninja",
        ]
    );
```

- [ ] **Step 3: Regenerate `deploy/crd.yaml`**

Run: `cargo run -q --bin crdgen > deploy/crd.yaml`

- [ ] **Step 4: Run the bootstrap-manifest tests to verify the regenerated file matches**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -20`
Expected: PASS — `crd_yaml_defines_all_platform_resources` and `crd_yaml_matches_the_generated_crds` both pass.

- [ ] **Step 5: Add the sixth watcher and Controller in `main.rs`**

Add the import near the top of `src/main.rs`, alongside the other component imports:

```rust
use platform_controller::snapshot_controller::SnapshotController;
use platform_controller::snapshot_controller_reconciler;
```

Change the `cert_manager_controller`'s own `.run(...)` call to clone `context` instead of moving it (it is used again below now):

```rust
    let cert_manager_controller = Controller::for_stream(cert_manager_installations, cert_manager_reader)
        .run(
            cert_manager_reconciler::reconcile_with_finalizer,
            cert_manager_reconciler::error_policy,
            context.clone(),
        )
```

Add, after the `cert_manager_controller` block and before `let mut sigterm = signal(...)`:

```rust
    // The snapshot-controller component gets its own watcher, store and
    // Controller too, with the same predicate filter and the same Context
    // (one leader lease).
    let snapshot_controller_api: Api<SnapshotController> = Api::all(client.clone());
    let (snapshot_controller_reader, snapshot_controller_writer) = reflector::store();
    let snapshot_controllers = watcher(snapshot_controller_api, watcher::Config::default())
        .default_backoff()
        .reflect(snapshot_controller_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let snapshot_controller_controller = Controller::for_stream(snapshot_controllers, snapshot_controller_reader)
        .run(
            snapshot_controller_reconciler::reconcile_with_finalizer,
            snapshot_controller_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled snapshot controller"),
                Err(err) => tracing::error!(error = %err, "snapshot controller reconcile failed"),
            }
        });
```

Add a sixth arm to the `tokio::select!` block:

```rust
        _ = snapshot_controller_controller => {}
```

- [ ] **Step 6: Add a deletion-requested unit test for the new kind, mirroring the existing five**

In `src/main.rs`'s `#[cfg(test)] mod tests`, add after `deletion_requested_works_for_the_cert_manager_installation_kind_too`:

```rust
    #[test]
    fn deletion_requested_works_for_the_snapshot_controller_kind_too() {
        let installation: SnapshotController = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "SnapshotController",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-29T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "chartVersion": "5.3.0"
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), Some(1));
    }
```

- [ ] **Step 7: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -40`
Expected: builds cleanly.

Run: `cargo test 2>&1 | tail -60`
Expected: PASS across the library, `main.rs`'s own tests, and every integration file (the ignored real-cluster/real-chart tests are skipped by default).

- [ ] **Step 8: Commit**

```bash
git add src/crds.rs src/main.rs tests/bootstrap_manifests.rs deploy/crd.yaml
git commit -m "$(cat <<'EOF'
feat: register SnapshotController's CRD and controller

Sixth CRD in crdgen's output; sixth watcher/Controller in main.rs,
sharing the existing leader lease and predicate filter. Regenerates
deploy/crd.yaml.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Example manifest

**Files:**
- Create: `examples/snapshot-controller.yaml`
- Create: `tests/snapshot_controller_example.rs`

**Interfaces:**
- Consumes: `platform_controller::snapshot_controller::{build_values, SnapshotController}`, `platform_controller::snapshot_controller_reconciler::validate`, `platform_controller::manifests::parse_manifests` (all from earlier tasks).
- Produces: nothing new — this task only adds a starting-point manifest and its test.

- [ ] **Step 1: Write the example manifest**

Create `examples/snapshot-controller.yaml`:

```yaml
# Sample SnapshotController for a self-hosted Talos Linux cluster: installs
# the cluster-wide CSI VolumeSnapshot support (the snapshot.storage.k8s.io/
# groupsnapshot.storage.k8s.io CRDs and the snapshot-controller itself) that
# CsiDriver's own csi-snapshotter sidecar needs to actually produce a
# VolumeSnapshot. Without this, the sidecar runs but every snapshot attempt
# just retries forever against CRDs that don't exist yet.
#
# This resource has a hard dependency on CertManagerInstallation already
# being Ready: it creates a self-signed cert-manager Issuer for the
# conversion webhook's TLS, which needs cert-manager's own Issuer CRD
# registered first. Applying this before CertManagerInstallation is Ready
# surfaces Failed/ApplyFailed and retries automatically once it catches up --
# no need to reapply in the right order, just wait.
#
#   kubectl apply -f examples/cert-manager.yaml
#   kubectl wait --for=jsonpath='{.status.phase}'=Ready certmgr/default --timeout=300s
#   kubectl apply -f examples/snapshot-controller.yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: SnapshotController
metadata:
  name: default
spec:
  platformKind: talos-linux
  # The Helm CHART version, not the app version (v8.6.0 at this chart
  # version) -- chart and app versions do not track together here, unlike
  # cert-manager.
  chartVersion: "5.3.0"
```

- [ ] **Step 2: Write the example test**

Create `tests/snapshot_controller_example.rs`:

```rust
use platform_controller::manifests::parse_manifests;
use platform_controller::snapshot_controller::{build_values, SnapshotController};
use platform_controller::snapshot_controller_reconciler;

const EXAMPLE: &str = "examples/snapshot-controller.yaml";

fn load() -> SnapshotController {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a SnapshotController")
}

#[test]
fn example_is_a_single_snapshot_controller_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "SnapshotController");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    snapshot_controller_reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_values_force_install_crds_webhook_and_the_selfsigned_issuer() {
    let values = build_values(&load().spec);

    assert_eq!(values["installCRDs"], true);
    assert_eq!(values["webhook"]["enabled"], true);
    assert_eq!(
        values["webhook"]["tls"]["certManagerIssuerRef"]["name"],
        platform_controller::snapshot_controller::SNAPSHOT_CONTROLLER_ISSUER_NAME
    );
}
```

- [ ] **Step 3: Run the example tests**

Run: `cargo test --test snapshot_controller_example 2>&1 | tail -20`
Expected: PASS, 3 tests.

- [ ] **Step 4: Commit**

```bash
git add examples/snapshot-controller.yaml tests/snapshot_controller_example.rs
git commit -m "$(cat <<'EOF'
feat: add the SnapshotController example manifest

A Talos starting point (chartVersion 5.3.0), with a comment on the
hard CertManagerInstallation apply-order dependency, plus a test that
it parses, validates and builds the expected values.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Integration test, docs, RBAC ledger and memory

**Files:**
- Create: `tests/integration_snapshot_controller.rs`
- Modify: `deploy/README.md`
- Create: `docs/runbooks/snapshot-controller-verification.md`
- Create: `docs/memory/snapshot-controller-2026-09.md`
- Modify: `docs/memory/MEMORY.md`, `docs/memory/csi-snapshot-support-2026-09.md`, `docs/memory/rbac-cluster-admin-tradeoff.md`

**Interfaces:**
- Consumes: `platform_controller::snapshot_controller::SnapshotController`, `platform_controller::crd::Phase` (Tasks 1, 4).
- Produces: nothing new — this task only adds the ignored integration test, deployment docs, the runbook and the memory entries.

- [ ] **Step 1: Write the ignored integration test**

Create `tests/integration_snapshot_controller.rs`:

```rust
// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running, all six CRDs Established, and CertManagerInstallation
// already Ready (this component has a hard dependency on it -- see
// examples/snapshot-controller.yaml):
//
//   kubectl apply -f examples/cert-manager.yaml
//   kubectl wait --for=jsonpath='{.status.phase}'=Ready certmgr/default --timeout=300s
//   kubectl apply -f examples/snapshot-controller.yaml
//   cargo test --test integration_snapshot_controller -- --ignored --nocapture
//
// The test deletes the SnapshotController at the end, so re-apply the
// example to run it again. It does NOT exercise the Issuer/Certificate
// smoke test or a real VolumeSnapshot; those are manual runbook steps
// (docs/runbooks/snapshot-controller-verification.md).

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::crd::Phase;
use platform_controller::snapshot_controller::SnapshotController;
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} did not happen within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
#[ignore = "requires a real cluster with the controller running; see module docs"]
async fn snapshot_controller_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let installations: Api<SnapshotController> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "snapshot-controller");
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());

    eventually("SnapshotController reaching Ready", Duration::from_secs(300), || async {
        installations
            .get("default")
            .await
            .ok()
            .and_then(|installation| installation.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    let listed = deployments
        .list(&ListParams::default())
        .await
        .expect("should list Deployments in the snapshot-controller namespace");
    assert_eq!(
        listed.items.len(),
        2,
        "Ready but the expected 2 Deployments (controller, conversion-webhook) are not both present: {:?}",
        listed.items.iter().filter_map(|d| d.metadata.name.clone()).collect::<Vec<_>>()
    );

    installations
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the SnapshotController");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        installations.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the snapshot-controller namespace disappearing", Duration::from_secs(120), || async {
        namespaces.get_opt("snapshot-controller").await.expect("get_opt should succeed").is_none()
    })
    .await;
    // installCRDs: true put this CRD in the ledger too -- confirms the
    // cascade-delete behavior documented in deploy/README.md and the runbook
    // is real, not just a claim.
    eventually("the volumesnapshots CRD disappearing", Duration::from_secs(120), || async {
        crds.get_opt("volumesnapshots.snapshot.storage.k8s.io")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
}
```

- [ ] **Step 2: Verify it compiles (it will not run without a live cluster)**

Run: `cargo test --test integration_snapshot_controller --no-run 2>&1 | tail -20`
Expected: compiles with no errors; no tests are executed (`--no-run`).

- [ ] **Step 3: Update `deploy/README.md`**

Add a sixth `kubectl wait` line after the `certmanagerinstallations` line in the top apply-order block:

```
kubectl wait --for=condition=established --timeout=60s crd/snapshotcontrollers.platform.rye.ninja
```

Update every `**Upgrading an existing install:**` note in the file to say "all six CRDs" instead of "all five CRDs".

Add a new section after "## Cert-manager (optional)" and before "## Calico node address autodetection":

```markdown
## Cluster-wide CSI snapshot support (optional)

`examples/snapshot-controller.yaml` is a `SnapshotController` that installs
the cluster-wide `snapshot.storage.k8s.io`/`groupsnapshot.storage.k8s.io`
CRDs and the `snapshot-controller` itself, from
[piraeusdatastore/helm-charts](https://github.com/piraeusdatastore/helm-charts).
This is the piece `CsiDriver`'s own `csi-snapshotter` sidecar needs but does
not install itself (see that section above): without it, the sidecar runs
but every `VolumeSnapshot` attempt just retries forever against CRDs that
don't exist.

This resource has a **hard dependency on `CertManagerInstallation`** being
`Ready` first: it creates a self-signed `cert-manager.io` `Issuer` for the
conversion webhook's TLS (group-snapshot support is on by default, which
needs the webhook), and that `Issuer`'s own CRD only exists once
`CertManagerInstallation` has been applied. Applying this first surfaces
`Failed`/`ApplyFailed` and retries automatically once
`CertManagerInstallation` catches up -- reapplying in the right order isn't
necessary, just waiting.

`spec.chartVersion` is the Helm chart version (`5.3.0`). Like the OpenStack
charts and unlike cert-manager, chart and app versions do **not** track
together (chart `5.3.0` ships app `v8.6.0`).

**Apply order:** after `CertManagerInstallation` and `CniInstallation` are
both `Ready`. No dependency on `CloudControllerManager` or `CsiDriver`,
though installing this is what makes `CsiDriver`'s `csi-snapshotter` log
noise stop.

**Deleting** a `SnapshotController` removes the chart's objects,
**including its six CRDs** (`volumesnapshots.snapshot.storage.k8s.io`,
`volumesnapshotclasses.snapshot.storage.k8s.io`, etc.). Kubernetes deletes
every instance of a kind when its CRD is deleted, so this destroys **every**
`VolumeSnapshot`/`VolumeSnapshotContent`/`VolumeSnapshotClass`/
`VolumeGroupSnapshot*` in the cluster along with it -- not just ones this
resource manages. Back up or export anything you need before deleting.
```

- [ ] **Step 4: Verify `deploy/README.md`'s code fences still match reality**

Run: `grep -c "snapshotcontrollers.platform.rye.ninja" deploy/README.md`
Expected: `1`.

- [ ] **Step 5: Write the runbook**

Create `docs/runbooks/snapshot-controller-verification.md`:

```markdown
# Verifying cluster-wide CSI snapshot support on Talos

Manual acceptance for the `SnapshotController` resource. Needs a real
cluster with `CertManagerInstallation` and `CniInstallation` already
`Ready`. For step 4, `CsiDriver` (OpenStack Cinder) must also be `Ready`
with at least one bound PVC. Nothing here has been run yet: record what you
observe under "Findings to record" at the end.

## 1. Apply and reach Ready

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/snapshotcontrollers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cert-manager.yaml
kubectl wait --for=jsonpath='{.status.phase}'=Ready certmgr/default --timeout=300s
kubectl apply -f examples/snapshot-controller.yaml
kubectl get snapctl default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n snapshot-controller get pods -o wide
```

Expected: `Ready`; two pods (`snapshot-controller`,
`snapshot-controller-conversion-webhook`), both Running.

## 2. Namespace admission under the default (`baseline`) Pod Security Standard

The design assumes this chart's pods need no `pod-security.kubernetes.io/*`
labels on their namespace, based on rendering the chart (no hostPath, no
hostNetwork), but — unlike cert-manager's chart — it sets no explicit
`seccompProfile`, so this is a weaker assumption than cert-manager's. Confirm
the pods actually started with no admission rejection:

```sh
kubectl get namespace snapshot-controller -o jsonpath='{.metadata.labels}{"\n"}'   # no pod-security labels
kubectl -n snapshot-controller get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
```

Expected: no `pod-security.kubernetes.io/*` label on the namespace; both
pods `Running`. If either is stuck `Pending` with a Pod Security admission
error, `snapshot_controller_namespace_object` in
`src/snapshot_controller_reconciler.rs` needs the same `privileged` labels
Calico's and Spegel's namespaces carry, and this runbook and the design spec
both need updating to say so.

## 3. The self-signed Issuer and the webhook's Certificate

```sh
kubectl -n snapshot-controller get issuer snapshot-controller-selfsigned -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl -n snapshot-controller get certificate snapshot-controller-conversion-webhook -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl -n snapshot-controller get secret snapshot-controller-conversion-webhook
kubectl get crd volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io -o jsonpath='{.spec.conversion.strategy}{"\n"}'   # Webhook
```

Expected: `Ready=True` on both the `Issuer` and the `Certificate`; the TLS
Secret exists; the group-snapshot CRD's conversion strategy is `Webhook`
(confirming the chart wired the webhook into the CRD, not just deployed a
pod). If the `Issuer`/`Certificate` never reach `Ready`, `CertManagerInstallation`
likely isn't actually healthy even though it reports `Ready` (which only
ever means "manifests applied") — check its own pods.

## 4. A real VolumeSnapshot against a Cinder PVC

Requires `CsiDriver` (OpenStack Cinder) `Ready` and an existing, bound PVC
(`docs/runbooks/csi-driver-openstack-cinder-verification.md`).

```sh
kubectl apply -f - <<'EOF'
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: smoketest-snapshot
  namespace: default
spec:
  volumeSnapshotClassName: csi-cinder-snapclass
  source:
    persistentVolumeClaimName: <an existing bound PVC's name>
EOF
```

`csi-cinder-snapclass` does not exist yet — this component does not create
any `VolumeSnapshotClass` (typed `VolumeSnapshotClass` support on `CsiDriver`
is out of scope, see the design spec's Non-goals). Create one manually first:

```sh
kubectl apply -f - <<'EOF'
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshotClass
metadata:
  name: csi-cinder-snapclass
driver: cinder.csi.openstack.org
deletionPolicy: Delete
EOF
kubectl get volumesnapshot smoketest-snapshot -o jsonpath='{.status.readyToUse}{"\n"}'
```

Expected: `true` within a couple of minutes. Confirm the underlying Cinder
snapshot exists via the Cinder API directly (`openstack volume snapshot
list`), the same live-verification standard the Cinder PVC provisioning
claim already met — not just Kubernetes-side status. Clean up:

```sh
kubectl delete volumesnapshot smoketest-snapshot
kubectl delete volumesnapshotclass csi-cinder-snapclass
```

## 5. Delete, and what stays

```sh
kubectl delete snapctl default        # returns once the finalizer clears
kubectl get namespace snapshot-controller    # NotFound
kubectl get crd volumesnapshots.snapshot.storage.k8s.io   # NotFound
```

Expected: the chart's objects, **including its six CRDs**, are gone.
Kubernetes deletes every instance of a kind when its CRD is deleted, so this
destroys **every** `VolumeSnapshot`/`VolumeSnapshotContent`/
`VolumeSnapshotClass`/`VolumeGroupSnapshot*` in the cluster, not just the
smoke test's own — confirm the smoke test's own resources were already
deleted at the end of step 4.

## When something goes wrong

- `kubectl get snapctl default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidHelmValues`,
  `Unsupported`; `RenderFailed` when helm cannot render the chart;
  `InvalidManifest`; `ApplyFailed` — including the case where
  `CertManagerInstallation` isn't `Ready` yet, since the `Issuer`'s
  `cert-manager.io/v1` kind isn't registered) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is
  applied, so deleting the resource after a failed first install still
  removes everything that was created.

## Findings to record

To fill in from the first live run: whether the namespace-admission
assumption in step 2 held, the Issuer/Certificate/conversion-strategy result
in step 3, the VolumeSnapshot result in step 4 (Kubernetes-side and Cinder
API-side), and anything in step 5 that differs from "Expected".
```

- [ ] **Step 6: Write the memory entry**

Create `docs/memory/snapshot-controller-2026-09.md`:

```markdown
---
name: snapshot-controller-2026-09
description: SnapshotController slice, 2026-09-29 - sixth CRD, resumes the queued csi-snapshot-support sub-project's CRD/controller/webhook half; chart facts confirmed by live rendering (not yet run against a cluster)
metadata:
  type: project
---

`SnapshotController` (cluster-scoped singleton `default`, shortname
`snapctl`) installs the cluster-wide CSI VolumeSnapshot support -- the
`snapshot.storage.k8s.io`/`groupsnapshot.storage.k8s.io` CRDs and the
`snapshot-controller` itself -- that [[csi-driver-openstack-cinder-2026-09]]'s
own `csi-snapshotter` sidecar needs but does not install. Spec:
`docs/superpowers/specs/2026-09-29-snapshot-controller-design.md`; plan:
`docs/superpowers/plans/2026-09-29-snapshot-controller.md`; live acceptance:
`docs/runbooks/snapshot-controller-verification.md`. Resumes
[[csi-snapshot-support-2026-09]]'s CRD/controller/webhook half -- see that
memory for what changed from its research (a real Helm chart exists after
all; group-snapshot support and the conversion webhook are in scope here,
which that research had leaned toward skipping).

**Non-obvious facts, from rendering the real chart (`5.3.0`), not assumed:**
- `kubernetes-csi/external-snapshotter` genuinely has no official Helm
  chart, but `piraeusdatastore/helm-charts`' `snapshot-controller` chart
  (classic repo `https://piraeus.io/helm-charts/`) is a real, actively
  maintained third-party one sourced directly from that upstream project --
  the existing `helm.rs` render pipeline (`ChartSource::Repo`) needed zero
  new code to support it, contradicting the queued sub-project's
  "needs a new raw-YAML render path" assumption.
- `installCRDs: true` renders exactly 6 CRDs: the 3 core
  (`volumesnapshotclasses`/`volumesnapshots`/`volumesnapshotcontents`.
  `snapshot.storage.k8s.io`) plus 3 group-snapshot ones
  (`volumegroupsnapshotclasses`/`volumegroupsnapshotcontents`/
  `volumegroupsnapshots`.`groupsnapshot.storage.k8s.io`) -- the queued
  research had recommended skipping the latter three as YAGNI; the approved
  design chose to include them instead, since the chart's own default
  (`--feature-gates=CSIVolumeGroupSnapshot=true`) already turns them on and
  excluding them would mean actively fighting the chart.
- The chart renders no `Namespace` and no `Secret` (the webhook's TLS Secret
  is produced later, at runtime, by cert-manager's own `Certificate`
  controller, not by `helm template`).
- Exactly 2 Deployments (`snapshot-controller`,
  `snapshot-controller-conversion-webhook`), 1 Service (webhook only -- the
  controller serves no traffic), 1 ClusterRole/ClusterRoleBinding pair, 1
  Role/RoleBinding pair (leader-election `Lease` access), 2 ServiceAccounts.
- The chart's "webhook" is a CRD **conversion** webhook (wired via each
  CRD's own `spec.conversion.webhook`), not a separate
  `Validating`/`MutatingWebhookConfiguration` object -- it only matters for
  clusters holding `VolumeGroupSnapshot` objects in old v1beta1/v1beta2 API
  versions, which a fresh install never has. Enabled anyway here (group
  snapshots in scope), following the chart's own README recipe for
  cert-manager-backed TLS: a namespaced self-signed `Issuer`, not a
  `ClusterIssuer` -- this component applies that `Issuer` itself, tracked in
  its own ledger, not shared with [[cert-manager-2026-09]] (which
  deliberately configures none).
- **This is the first component with a hard, same-reconcile dependency on
  another component's CRDs actually being registered**, unlike every prior
  ordering note in this codebase (all of which were "eventually consistent"
  Pod-scheduling delays, not apply-time failures). Handled with zero new
  code: the `Issuer` (hand-built) and the chart's own `Certificate` are both
  `cert-manager.io` custom resources, so the existing
  `is_custom_resource`/`wait_for_object_kind` mechanism already used for
  `CsiDriver`'s `StorageClass`/`CSIDriver` objects and `CniInstallation`'s
  wait on the tigera operator's own CRDs covers this for free. Applying
  `SnapshotController` before [[cert-manager-2026-09]] surfaces
  `Failed`/`ApplyFailed` (not a bespoke reason) and retries automatically.
- Controller and webhook containers drop every capability and run
  non-root, but -- unlike cert-manager's chart -- set no explicit
  `seccompProfile`. The synthesized namespace is designed to carry no
  `pod-security.kubernetes.io/*` labels anyway (matching cert-manager's
  namespace), but this is a **stated assumption pending the runbook's step
  2**, weaker than cert-manager's live-confirmed one.
- Chart and app versions do **not** track together (`5.3.0` ships app
  `v8.6.0`), like the OpenStack charts and unlike cert-manager.
- This is a classic-repo chart (`ChartSource::Repo`), not OCI, so
  `strip_oci_pull_preamble` never applies to it.
- A `helmValues` attempt to set `installCRDs`, `webhook.enabled` or anything
  under `webhook.tls` is **rejected** at validation
  (`InvalidHelmValues`), unlike `CertManagerInstallation`'s `crds.enabled`
  (silently overridden) -- a webhook TLS misconfiguration fails loudly here
  instead of quietly.
- Deleting a `SnapshotController` cascades to delete all six CRDs and
  therefore every `VolumeSnapshot`/`VolumeGroupSnapshot`-family object
  cluster-wide, the same class of hazard [[cert-manager-2026-09]] already
  documented for its own six CRDs.
- No typed `VolumeSnapshotClass` support here (or on `CsiDriver`) --
  deliberately descoped during brainstorming; see the design spec's
  Non-goals. The runbook's live `VolumeSnapshot` smoke test (step 4) has to
  create one by hand for that reason.

**Verification status:** not yet implemented against real code (this memory
was written alongside the plan). Nothing has been run against an actual
cluster. Update this entry once Tasks 1-6 are implemented and the runbook is
run.

**How to apply:** when bumping the chart version, re-render it
(`helm template snapshot-controller --repo https://piraeus.io/helm-charts/
snapshot-controller --version <new> --include-crds --no-hooks --namespace
snapshot-controller --values <the same typed values build_values sets>`)
and re-check the CRD list, the Deployment/Service/Certificate shape, and the
feature-gate default; re-run the ignored real-chart test in `helm.rs`.
```

- [ ] **Step 7: Update `docs/memory/csi-snapshot-support-2026-09.md`**

The queued sub-project this slice resumes needs its frontmatter and an
opening note updated so it stops reading as unstarted. Read the file first,
then:

- Change the `description` field to: `Sub-project B, split 2026-09-29: the CRD/controller/webhook half shipped as SnapshotController (see [[snapshot-controller-2026-09]]); typed VolumeSnapshotClass support on CsiDriver remains genuinely queued`
- Add one paragraph immediately after the frontmatter, before the existing "Sub-project B of an original request..." paragraph:

```markdown
**Split 2026-09-29.** The CRD/controller/cluster-wide `snapshot-controller`
half of this sub-project shipped as its own component,
[[snapshot-controller-2026-09]] -- see that memory and
`docs/superpowers/specs/2026-09-29-snapshot-controller-design.md` for what
was actually built, including two corrections to the research below (a real
Helm chart does exist; group-snapshot support was included rather than
skipped). The typed-`VolumeSnapshotClass`-on-`CsiDriver` half described
below (naming convention, `storageClasses.additional[]`-shaped
`helmValues`) remains genuinely queued and unbuilt.
```

Leave the rest of the file (the original research) as-is — it is still
useful history for whoever picks up the `VolumeSnapshotClass` half.

- [ ] **Step 8: Update the memory index**

In `docs/memory/MEMORY.md`, replace the existing `[CSI snapshot support (queued)]` line with two lines (keep them adjacent, after the `CertManagerInstallation` line):

```markdown
- [CSI snapshot support, VolumeSnapshotClass half (queued)](csi-snapshot-support-2026-09.md) — split 2026-09-29; typed VolumeSnapshotClass support on CsiDriver still unbuilt; naming convention and shared-CRD research here now historical
- [SnapshotController slice](snapshot-controller-2026-09.md) — sixth CRD; installs the CRD/controller/webhook half; hard same-reconcile dependency on CertManagerInstallation via the existing wait_for_object_kind mechanism; deleting it cascades all six CRDs
```

- [ ] **Step 9: Add an RBAC ledger row**

Append a row to the table in `docs/memory/rbac-cluster-admin-tradeoff.md` (after the CSI driver row), and add one sentence to the file's running notes if useful context is missing:

```markdown
| `""` (core), `apps`, `rbac.authorization.k8s.io`, `snapshot.storage.k8s.io`, `groupsnapshot.storage.k8s.io` | `persistentvolumes`, `persistentvolumeclaims`, `events` in core; `deployments` in `apps`; `clusterroles`/`clusterrolebindings`/`roles`/`rolebindings`; full CRUD on `volumesnapshotclasses`/`volumesnapshotcontents`/`volumesnapshots` and their `groupsnapshot.storage.k8s.io` counterparts, plus their `/status` subresources | snapshot-controller chart (`5.3.0`, rendered with `--no-hooks`) | Applies no kinds the controller does not already apply. Because of privilege-escalation prevention the controller must hold every one of these to create the chart's ClusterRole/Role. |
| `cert-manager.io` | `issuers` (create/get/update/patch/delete), `certificates` (get, to discover the kind before applying the chart's own Certificate) | Controller itself | The self-signed `Issuer` this component applies directly (not from any chart) for the conversion webhook's TLS; the chart's own `Certificate` object is applied through the same generic apply path as everything else. |
```

- [ ] **Step 10: Run the full test suite and clippy one more time**

Run: `cargo test 2>&1 | tail -60`
Expected: PASS.

Run: `cargo clippy --all-targets 2>&1 | tail -60`
Expected: only the same warning classes the existing code already triggers (`result_large_err`, `too_many_arguments`, and any pre-existing ones in `apply.rs`/`manifests.rs`) — no new categories.

- [ ] **Step 11: Commit**

```bash
git add tests/integration_snapshot_controller.rs deploy/README.md \
  docs/runbooks/snapshot-controller-verification.md \
  docs/memory/snapshot-controller-2026-09.md docs/memory/MEMORY.md \
  docs/memory/csi-snapshot-support-2026-09.md docs/memory/rbac-cluster-admin-tradeoff.md
git commit -m "$(cat <<'EOF'
docs: add snapshot-controller deployment docs, runbook and memory entry

Ignored integration test (apply-after-CertManagerInstallation,
Ready/delete/cascade-cleanup); deploy/README.md gains a cluster-wide
CSI snapshot support section and the sixth CRD's establish-wait; a
runbook covering namespace admission, the Issuer/Certificate/
conversion-webhook wiring, and a real VolumeSnapshot against a Cinder
PVC; a memory entry recording the live-rendered chart facts this plan
relied on; and the queued csi-snapshot-support-2026-09 memory marked
split, with the CRD/controller/webhook half pointed at this one.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
