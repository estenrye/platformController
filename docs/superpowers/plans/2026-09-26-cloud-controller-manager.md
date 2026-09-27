# Cloud Controller Manager (OpenStack) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a third platform component to the controller: a `CloudControllerManager` custom resource that installs cloud-provider-openstack on self-hosted Talos clusters running on OpenStack VMs.

**Architecture:** A new cluster-scoped singleton CRD, `CloudControllerManager`, with its own reconciler (`src/ccm_reconciler.rs`) that renders the `openstack-cloud-controller-manager` Helm chart from its classic chart repository, server-side-applies it into `kube-system`, and prunes and cleans up through the same primitives the other two reconcilers use. The OpenStack credentials stay in a user-created Secret that the CR only names. `main.rs` runs a third `Controller` under the same leader lease. The controller's own Deployment gains a toleration for the `uninitialized` cloud-provider taint so it can schedule on nodes the CCM has not initialized yet.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, the `helm` CLI (already a runtime dependency), chart `openstack-cloud-controller-manager` `2.36.5` (app `v1.36.0`) from `https://kubernetes.github.io/cloud-provider-openstack`.

**Spec:** [docs/superpowers/specs/2026-09-26-cloud-controller-manager-design.md](../specs/2026-09-26-cloud-controller-manager-design.md)

## Global Constraints

Every task's requirements implicitly include this section. Values are copied from the spec, plus facts confirmed by rendering the real chart while writing this plan.

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `CloudControllerManager`, **cluster-scoped**, shortname `ccm`, plural `cloudcontrollermanagers`, singleton named `default`.
- `platformKind` accepts only `talos-linux`; `provider` accepts only `openstack`. There is no provider check in code (single-variant enum; serde rejects anything else).
- `chartVersion` is the Helm **chart** version (`2.36.5`), not the app version (`v1.36.0`). It is required, has no default, and is passed verbatim to `helm --version`, so leading or trailing whitespace is rejected.
- `cloudConfigSecretRef.name` is required and must be a valid Kubernetes object name (DNS-1123 subdomain). The Secret lives in `kube-system` (the chart mounts it from its release namespace) and must hold the whole cloud config under the key `cloud.conf`. The controller never reads it.
- Values layering: `helmValues` first, then the typed fields (`secret.enabled: true`, `secret.create: false`, `secret.name: <ref>`), then the unconditional Talos overrides (`extraVolumes: []`, `extraVolumeMounts: []`). Later layers always win.
- Chart: repo `https://kubernetes.github.io/cloud-provider-openstack`, chart `openstack-cloud-controller-manager`, release name `openstack-ccm` (it appears in the DaemonSet's immutable selector, so never change it), namespace `kube-system`. Render with `--include-crds --no-hooks --namespace kube-system`.
- No namespace is synthesized (`kube-system` exists and Talos's default admission config exempts it). The ledger therefore holds only rendered objects.
- No `cleanupTimeoutSeconds` on this CRD.
- The finalizer name `platform.rye.ninja/cleanup` is reused (finalizers are per object).
- `enabledControllers` is left at the chart default (`cloud-node`, `cloud-node-lifecycle`, `route`, `service`); the `route` controller's behavior under Calico is a live-verification item, not a code change.
- The Secret's existence is not checked. `Ready` means "manifests applied", not "CCM healthy".
- `deploy/bootstrap.yaml` gains a toleration for `node.cloudprovider.kubernetes.io/uninitialized` (`operator: Exists`, effect `NoSchedule`).
- The three components apply in this documented order: `CloudControllerManager`, `CniInstallation`, `PullThroughCache`. The controller enforces no ordering.
- Existing CNI and cache tests must pass unchanged, except `crd_yaml_defines_both_platform_resources`, which is renamed and extended to three CRDs. The CNI and cache reconcile/cleanup logic is not modified; `src/pull_through_cache.rs` gets exactly two visibility changes (Task 1).
- CI runs `cargo build`, `cargo test`, `cargo clippy --all-targets`. Clippy warnings are not denied, and `result_large_err` already fires on the existing reconcilers; the new one triggers it the same way and that is accepted.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Building needs free disk (a debug build of this crate is several GB). If a build fails with `No space left on device`, free space first; do not delete anything that is not a build artifact.
- CLAUDE.md: project memory lives in `docs/memory/` (index in `docs/memory/MEMORY.md`), committed like any other change. Do not write to the out-of-repo memory path.

## Review Focus

Failure modes the spec implies but that are easiest to miss. Each has a test in the task that owns the code.

1. A Secret name a person would plausibly type but Kubernetes rejects (`Cloud-Config`, `cloud_config`, trailing dot, leading or trailing space, empty, over 253 characters). It must fail validation with a reason, not fail later in the DaemonSet's mount. Test in Task 1.
2. A `helmValues` passthrough that tries to undo a typed or platform value (`secret.create: true`, a different `secret.name`, its own `extraVolumes`). The typed and Talos layers must win. Tests in Task 1.
3. A chart bump that changes the rendered shape the spec relies on (a Secret object appears, hostPath volumes return, the Secret is mounted under a different key or name, a hook is rendered). The ignored real-chart test in Task 2 pins it; re-run it on every chart bump.
4. A second `CloudControllerManager` with any name other than `default`. It must get `Failed` / `Unsupported`, not be reconciled. Test in Task 3.
5. A standby replica, or a validation failure, must not overwrite status with a generic failure. Test in Task 3.

---

## Branch

The spec and this plan are committed on `main`. Create the implementation branch from there:

```bash
git checkout -b cloud-controller-manager
```

## Verification status of the code in this plan

All code in Tasks 1-6 was extracted from this plan into a scratch worktree of `main` and run on 2026-09-26: `cargo build` succeeded; the full `cargo test` passed (180 unit tests in the library, 4 in `main.rs`; `tests/bootstrap_manifests.rs` 5, `tests/cloud_controller_manager_example.rs` 4; the new integration test compiles and is ignored); the real-chart test `helm::tests::openstack_ccm_chart_renders_the_shape_the_spec_relies_on` passed against the live chart repository; and `cargo clippy --all-targets` reported only warning classes the existing code already triggers (`result_large_err`, `too_many_arguments`, and two pre-existing ones in `apply.rs`/`manifests.rs`).

What has **not** been run: the manual live-cluster steps (the runbook, Task 6) and the ignored Talos-in-Docker integration test. If any code block here fails to compile when you apply it, treat that as a plan bug to fix, not to work around.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/cloud_controller_manager.rs` (create) | `CloudControllerManager` CRD types, status type, spec validation, the values builder. |
| `src/ccm_reconciler.rs` (create) | Reconcile, cleanup, finalizer wiring, status for `CloudControllerManager`. |
| `src/pull_through_cache.rs` (modify, 2 words) | `merge` and `preserve_unknown_object` become `pub(crate)` so the new module reuses them. |
| `src/helm.rs` (modify) | `CCM_NAMESPACE`, `OPENSTACK_CCM_CHART_REPO`, `OPENSTACK_CCM_CHART`; render-args test; ignored real-chart test. |
| `src/lib.rs` (modify) | Register the two new modules. |
| `src/crds.rs` (modify) | Include the third CRD in `generated_yaml()`. |
| `src/main.rs` (modify) | Third watcher and `Controller<CloudControllerManager>`. |
| `deploy/crd.yaml` (regenerate) | All three CRDs. |
| `deploy/bootstrap.yaml` (modify) | Toleration for the `uninitialized` taint. |
| `deploy/README.md` (modify) | Establish-wait for the third CRD, apply order, upgrade note, CCM section. |
| `examples/cloud-controller-manager.yaml` (create) | Talos-on-OpenStack starting point. |
| `tests/bootstrap_manifests.rs` (modify) | Three CRDs; the toleration. |
| `tests/cloud_controller_manager_example.rs` (create) | The example parses, validates and builds the expected values. |
| `tests/integration_cloud_controller_manager.rs` (create) | Ignored: apply, assert the DaemonSet, delete, assert cleanup. |
| `docs/runbooks/cloud-controller-manager-verification.md` (create) | Node prerequisite and the live acceptance checks. |
| `docs/memory/cloud-controller-manager-2026-09.md`, `docs/memory/MEMORY.md`, `docs/memory/rbac-cluster-admin-tradeoff.md` | Memory entry, index line, ledger rows. |

---

### Task 1: CRD types, validation and values builder

**Files:**
- Create: `src/cloud_controller_manager.rs`
- Modify: `src/lib.rs`, `src/pull_through_cache.rs:49` and `:232`
- Test: inline `#[cfg(test)]` module in `src/cloud_controller_manager.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`; `crate::pull_through_cache::{merge, preserve_unknown_object}` (made `pub(crate)` in this task).
- Produces:
  - `CloudControllerManager` (the CRD kind), `CloudControllerManagerSpec { platform_kind: PlatformKind, provider: CloudProvider, openstack: OpenstackSpec }`, `CloudControllerManagerStatus { phase, observed_generation: i64, chart_version: String, applied_resources: Vec<AppliedResourceRef>, conditions: Vec<Condition> }`
  - `CloudProvider::Openstack`, `OpenstackSpec { chart_version: String, cloud_config_secret_ref: SecretNameRef, helm_values: Option<serde_json::Value> }` (derives `Default`), `SecretNameRef { name: String }`
  - `CcmSpecError` with `reason(&self) -> &'static str` returning `"InvalidChartVersion"`, `"InvalidSecretRef"` or `"InvalidHelmValues"`
  - `validate_openstack(&OpenstackSpec) -> Result<(), CcmSpecError>`
  - `build_values(&OpenstackSpec) -> serde_json::Value`

- [ ] **Step 1: Make the two shared helpers reusable**

In `src/pull_through_cache.rs`, change `fn preserve_unknown_object` (line 49) and `fn merge` (line 232) to `pub(crate) fn`. Nothing else in that file changes.

```rust
pub(crate) fn preserve_unknown_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
```

```rust
pub(crate) fn merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
```

- [ ] **Step 2: Write the failing tests**

Create `src/cloud_controller_manager.rs` containing only the test module below (the implementation follows in Step 4), and add `pub mod cloud_controller_manager;` to `src/lib.rs` after `pub mod cache_reconciler;`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn openstack() -> OpenstackSpec {
        OpenstackSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: SecretNameRef {
                name: "cloud-config".to_string(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn spec_deserializes_with_only_the_required_fields() {
        let spec: CloudControllerManagerSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "openstack",
            "openstack": {
                "chartVersion": "2.36.5",
                "cloudConfigSecretRef": { "name": "cloud-config" }
            }
        }))
        .expect("minimal spec should deserialize");

        assert_eq!(spec.platform_kind, PlatformKind::TalosLinux);
        assert_eq!(spec.provider, CloudProvider::Openstack);
        assert_eq!(spec.openstack.chart_version, "2.36.5");
        assert_eq!(spec.openstack.cloud_config_secret_ref.name, "cloud-config");
        assert!(spec.openstack.helm_values.is_none());
    }

    #[test]
    fn unknown_providers_are_rejected_at_deserialization() {
        let result = serde_json::from_value::<CloudControllerManagerSpec>(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "aws",
            "openstack": {
                "chartVersion": "2.36.5",
                "cloudConfigSecretRef": { "name": "cloud-config" }
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn a_spec_without_a_secret_reference_is_rejected_at_deserialization() {
        let result = serde_json::from_value::<CloudControllerManagerSpec>(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "openstack",
            "openstack": { "chartVersion": "2.36.5" }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn accepts_a_minimal_spec() {
        assert_eq!(validate_openstack(&openstack()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_chart_version() {
        let mut spec = openstack();
        spec.chart_version = "  ".to_string();

        let err = validate_openstack(&spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err, CcmSpecError::EmptyChartVersion);
        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn rejects_chart_versions_with_leading_or_trailing_whitespace() {
        for version in [" 2.36.5", "2.36.5 ", "2.36.5\n"] {
            let mut spec = openstack();
            spec.chart_version = version.to_string();

            let err = validate_openstack(&spec).expect_err("whitespace must be rejected");

            assert_eq!(err, CcmSpecError::ChartVersionHasWhitespace(version.to_string()));
            assert_eq!(err.reason(), "InvalidChartVersion");
        }
    }

    #[test]
    fn accepts_secret_names_kubernetes_accepts() {
        for name in ["cloud-config", "a", "my.secret-1", "0abc", &"a".repeat(253)] {
            let mut spec = openstack();
            spec.cloud_config_secret_ref.name = name.to_string();

            assert_eq!(validate_openstack(&spec), Ok(()), "{name}");
        }
    }

    #[test]
    fn rejects_secret_names_kubernetes_rejects() {
        for name in [
            "",
            "Cloud-Config",
            "cloud_config",
            "-cloud-config",
            "cloud-config-",
            ".cloud-config",
            "cloud-config.",
            "cloud..config",
            " cloud-config",
            "cloud-config\n",
            &"a".repeat(254),
        ] {
            let mut spec = openstack();
            spec.cloud_config_secret_ref.name = name.to_string();

            let err = validate_openstack(&spec).expect_err("invalid Secret name must be rejected");

            assert_eq!(err, CcmSpecError::InvalidSecretName(name.to_string()), "{name:?}");
            assert_eq!(err.reason(), "InvalidSecretRef");
        }
    }

    #[test]
    fn rejects_helm_values_that_are_not_an_object() {
        for value in [
            serde_json::json!("nope"),
            serde_json::json!(["a"]),
            serde_json::json!(3),
            serde_json::json!(null),
        ] {
            let mut spec = openstack();
            spec.helm_values = Some(value.clone());

            let err = validate_openstack(&spec).expect_err("non-object helmValues is invalid");

            assert_eq!(err, CcmSpecError::HelmValuesNotObject, "{value}");
            assert_eq!(err.reason(), "InvalidHelmValues");
        }
    }

    #[test]
    fn values_point_the_chart_at_the_existing_secret_and_never_create_one() {
        let values = build_values(&openstack());

        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["name"], "cloud-config");
    }

    #[test]
    fn values_always_drop_the_chart_host_path_volumes() {
        let values = build_values(&openstack());

        assert_eq!(values["extraVolumes"], serde_json::json!([]));
        assert_eq!(values["extraVolumeMounts"], serde_json::json!([]));
    }

    #[test]
    fn typed_and_talos_values_win_over_conflicting_helm_values() {
        let mut spec = openstack();
        spec.helm_values = Some(serde_json::json!({
            "secret": { "create": true, "enabled": false, "name": "other" },
            "extraVolumes": [{ "name": "x" }],
            "extraVolumeMounts": [{ "name": "x" }]
        }));

        let values = build_values(&spec);

        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["name"], "cloud-config");
        assert_eq!(values["extraVolumes"], serde_json::json!([]));
        assert_eq!(values["extraVolumeMounts"], serde_json::json!([]));
    }

    #[test]
    fn helm_values_pass_through_alongside_the_typed_values() {
        let mut spec = openstack();
        spec.helm_values = Some(serde_json::json!({
            "cluster": { "name": "prod" },
            "logVerbosityLevel": 4,
            "secret": { "annotations": { "a": "b" } }
        }));

        let values = build_values(&spec);

        assert_eq!(values["cluster"]["name"], "prod");
        assert_eq!(values["logVerbosityLevel"], 4);
        // Merging is recursive: untouched siblings under `secret` survive.
        assert_eq!(values["secret"]["annotations"]["a"], "b");
        assert_eq!(values["secret"]["name"], "cloud-config");
    }

    #[test]
    fn crd_is_cluster_scoped_with_the_ccm_shortname() {
        let crd = CloudControllerManager::crd();

        assert_eq!(
            crd.metadata.name.as_deref(),
            Some("cloudcontrollermanagers.platform.rye.ninja")
        );
        assert_eq!(crd.spec.scope, "Cluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["ccm".to_string()]));
    }

    #[test]
    fn crd_schema_lets_helm_values_carry_arbitrary_keys() {
        let yaml = serde_yaml::to_string(&CloudControllerManager::crd()).expect("CRD serializes");

        assert!(
            yaml.contains("x-kubernetes-preserve-unknown-fields: true"),
            "helmValues must be a structural, unknown-field-preserving object:\n{yaml}"
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib cloud_controller_manager 2>&1 | tail -20`
Expected: FAIL to compile with errors such as ``cannot find type `OpenstackSpec` in this scope``.

- [ ] **Step 4: Write the implementation**

Put this above the test module in `src/cloud_controller_manager.rs`:

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CloudControllerManager",
    status = "CloudControllerManagerStatus",
    shortname = "ccm"
)]
#[serde(rename_all = "camelCase")]
pub struct CloudControllerManagerSpec {
    pub platform_kind: PlatformKind,
    pub provider: CloudProvider,
    pub openstack: OpenstackSpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CloudProvider {
    Openstack,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenstackSpec {
    /// The Helm chart version of `openstack-cloud-controller-manager` (for
    /// example `2.36.5`), not the application version (`v1.36.0`).
    pub chart_version: String,
    /// The Secret holding the OpenStack cloud config. It must exist in
    /// `kube-system` and hold the whole config under the key `cloud.conf`. The
    /// controller passes only its name to the chart and never reads it.
    pub cloud_config_secret_ref: SecretNameRef,
    /// Free-form values merged into the chart's values. Typed fields and the
    /// controller's own Talos settings are overlaid on top, so they always win.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::pull_through_cache::preserve_unknown_object")]
    pub helm_values: Option<serde_json::Value>,
}

/// A reference to a Secret by name only: the namespace is fixed by the chart.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
pub struct SecretNameRef {
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudControllerManagerStatus {
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
pub enum CcmSpecError {
    #[error("spec.openstack.chartVersion must not be empty")]
    EmptyChartVersion,
    #[error("spec.openstack.chartVersion {0:?} has leading or trailing whitespace")]
    ChartVersionHasWhitespace(String),
    #[error(
        "spec.openstack.cloudConfigSecretRef.name {0:?} is not a valid Kubernetes object name \
         (lowercase alphanumerics, '-' and '.', starting and ending with an alphanumeric, at \
         most 253 characters)"
    )]
    InvalidSecretName(String),
    #[error("spec.openstack.helmValues must be a JSON object")]
    HelmValuesNotObject,
}

impl CcmSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            CcmSpecError::EmptyChartVersion | CcmSpecError::ChartVersionHasWhitespace(_) => {
                "InvalidChartVersion"
            }
            CcmSpecError::InvalidSecretName(_) => "InvalidSecretRef",
            CcmSpecError::HelmValuesNotObject => "InvalidHelmValues",
        }
    }
}

/// A Kubernetes object name: a DNS-1123 subdomain. One or more dot-separated
/// labels of lowercase alphanumerics and '-', each starting and ending with an
/// alphanumeric, at most 253 characters in all.
fn is_dns1123_subdomain(name: &str) -> bool {
    let alphanumeric = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            let bytes = label.as_bytes();
            bytes.first().is_some_and(alphanumeric)
                && bytes.last().is_some_and(alphanumeric)
                && bytes.iter().all(|b| alphanumeric(b) || *b == b'-')
        })
}

/// Rejects specs the chart would silently mis-handle, before anything is
/// applied.
pub fn validate_openstack(openstack: &OpenstackSpec) -> Result<(), CcmSpecError> {
    if openstack.chart_version.trim().is_empty() {
        return Err(CcmSpecError::EmptyChartVersion);
    }
    if openstack.chart_version.trim() != openstack.chart_version {
        return Err(CcmSpecError::ChartVersionHasWhitespace(
            openstack.chart_version.clone(),
        ));
    }
    if !is_dns1123_subdomain(&openstack.cloud_config_secret_ref.name) {
        return Err(CcmSpecError::InvalidSecretName(
            openstack.cloud_config_secret_ref.name.clone(),
        ));
    }
    if let Some(values) = &openstack.helm_values
        && !values.is_object()
    {
        return Err(CcmSpecError::HelmValuesNotObject);
    }
    Ok(())
}

/// The Helm values for the OpenStack CCM chart: the user's `helmValues`
/// passthrough, with the typed fields and the Talos overrides overlaid on top.
///
/// - The Secret is user-created, so the chart must use it and never create one.
/// - `extraVolumes` and `extraVolumeMounts` default to hostPath mounts of
///   `/etc/kubernetes/pki` and the kubelet flexvolume directory. The CCM uses
///   in-cluster config and needs neither, and Talos provides neither, so they
///   are always emptied (a user's own `extraVolumes` is overridden too).
pub fn build_values(openstack: &OpenstackSpec) -> serde_json::Value {
    let mut values = openstack
        .helm_values
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));

    let typed = serde_json::json!({
        "secret": {
            "enabled": true,
            "create": false,
            "name": openstack.cloud_config_secret_ref.name,
        },
        "extraVolumes": [],
        "extraVolumeMounts": [],
    });

    crate::pull_through_cache::merge(&mut values, typed);
    values
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cloud_controller_manager 2>&1 | tail -25`
Expected: PASS, 15 tests in `cloud_controller_manager::tests`. Then `cargo test --lib pull_through_cache 2>&1 | tail -5`: existing tests still pass.

- [ ] **Step 6: Commit**

```bash
git add src/cloud_controller_manager.rs src/lib.rs src/pull_through_cache.rs
git commit -m "$(cat <<'EOF'
feat: CloudControllerManager CRD types, validation and values builder

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Chart reference and real-chart render test

**Files:**
- Modify: `src/helm.rs` (constants after `SPEGEL_CHART`; tests in the existing `tests` module)
- Test: inline tests in `src/helm.rs`

**Interfaces:**
- Consumes: `ChartRef`, `ChartSource`, `render_args`, `render_chart` (existing); `crate::cloud_controller_manager::{build_values, OpenstackSpec, SecretNameRef}` (Task 1); `crate::manifests::parse_manifests`.
- Produces: `helm::CCM_NAMESPACE: &str`, `helm::OPENSTACK_CCM_CHART_REPO: &str`, `helm::OPENSTACK_CCM_CHART: ChartRef`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/helm.rs`, next to `oci_render_args_pass_the_reference_without_a_repo_flag`:

```rust
    #[test]
    fn openstack_ccm_render_args_use_the_repo_form_and_kube_system() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&OPENSTACK_CCM_CHART, "2.36.5", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "openstack-ccm".to_string(),
                "--repo".to_string(),
                "https://kubernetes.github.io/cloud-provider-openstack".to_string(),
                "openstack-cloud-controller-manager".to_string(),
                "--version".to_string(),
                "2.36.5".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "kube-system".to_string(),
            ]
        );
    }
```

And, next to the Spegel real-chart tests:

```rust
    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn openstack_ccm_chart_renders_the_shape_the_spec_relies_on() {
        let openstack = crate::cloud_controller_manager::OpenstackSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: crate::cloud_controller_manager::SecretNameRef {
                name: "my-cloud-config".to_string(),
            },
            ..Default::default()
        };
        let values = crate::cloud_controller_manager::build_values(&openstack);

        let rendered = render_chart(&OPENSTACK_CCM_CHART, &openstack.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().expect("every rendered object has a type").kind.as_str())
            .collect();
        // The controller never creates the credentials Secret; the user does.
        assert!(!kinds.contains(&"Secret"), "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| **kind == "DaemonSet").count(), 1, "{kinds:?}");

        let daemon_set = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "DaemonSet")
            .expect("one DaemonSet");
        assert_eq!(daemon_set.metadata.namespace.as_deref(), Some("kube-system"));

        // Only the cloud-config Secret volume remains: the hostPath mounts are gone.
        let volumes = daemon_set
            .data
            .pointer("/spec/template/spec/volumes")
            .and_then(|value| value.as_array())
            .expect("the DaemonSet has volumes");
        assert_eq!(volumes.len(), 1, "{volumes:?}");
        assert_eq!(volumes[0]["secret"]["secretName"], "my-cloud-config");
        assert!(!rendered.contains("hostPath"), "{rendered}");

        // The chart reads the config from the `cloud.conf` key of that Secret, and
        // its Role is scoped to the same name.
        assert!(rendered.contains("/etc/config/cloud.conf"), "{rendered}");
        assert!(rendered.contains("- my-cloud-config"), "{rendered}");

        // --no-hooks: no hook objects are rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib helm::tests::openstack_ccm 2>&1 | tail -15`
Expected: FAIL to compile: ``cannot find value `OPENSTACK_CCM_CHART` in this scope``.

- [ ] **Step 3: Write the implementation**

In `src/helm.rs`, after the `SPEGEL_NAMESPACE` constant add:

```rust
/// Namespace the OpenStack cloud-controller-manager chart is released into. The
/// namespace already exists and Talos's default admission configuration exempts
/// it from Pod Security, so unlike Spegel no namespace is synthesized.
pub const CCM_NAMESPACE: &str = "kube-system";

/// The Helm repository the OpenStack cloud-controller-manager chart is fetched from.
pub const OPENSTACK_CCM_CHART_REPO: &str = "https://kubernetes.github.io/cloud-provider-openstack";
```

And after `SPEGEL_CHART`:

```rust
/// The release name is part of the DaemonSet's immutable `selector` (the chart
/// labels pods `release: <name>`), so it must never change for an installed cluster.
pub const OPENSTACK_CCM_CHART: ChartRef = ChartRef {
    release: "openstack-ccm",
    source: ChartSource::Repo {
        url: OPENSTACK_CCM_CHART_REPO,
        chart: "openstack-cloud-controller-manager",
    },
    namespace: CCM_NAMESPACE,
};
```

- [ ] **Step 3b: Run the tests to verify they pass**

Run: `cargo test --lib helm::tests::openstack_ccm_render_args 2>&1 | tail -5`
Expected: PASS.

Run (needs network and `helm`): `cargo test --lib helm::tests::openstack_ccm_chart_renders -- --ignored 2>&1 | tail -15`
Expected: PASS. If it fails on an assertion, the chart's shape differs from what the spec records: stop and update the spec before continuing.

- [ ] **Step 4: Commit**

```bash
git add src/helm.rs
git commit -m "$(cat <<'EOF'
feat: OpenStack CCM chart reference and real-chart render test

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: The reconciler

**Files:**
- Create: `src/ccm_reconciler.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` module in `src/ccm_reconciler.rs`

**Interfaces:**
- Consumes: Task 1's types and `validate_openstack`, `build_values`; Task 2's `helm::OPENSTACK_CCM_CHART` and `helm::render_chart`; existing `crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME}`, `crate::apply::{apply_object, delete_object, resource_ref, resources_to_prune, wait_for_crd_established}`, `crate::ledger::{ReconcileProgress, checkpoint_ledger, failure_ledger}`, `crate::manifests::{parse_manifests, sort_manifests, is_custom_resource}`.
- Produces:
  - `ccm_reconciler::validate(name: &str, spec: &CloudControllerManagerSpec) -> Result<(), ValidationError>`
  - `ccm_reconciler::ValidationError` with `reason(&self) -> &'static str`
  - `ccm_reconciler::CcmReconcileError` with `failure_reason(&self) -> Option<&'static str>`
  - `ccm_reconciler::reconcile_with_finalizer(Arc<CloudControllerManager>, Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<CcmReconcileError>>` and `ccm_reconciler::error_policy(Arc<CloudControllerManager>, &finalizer::Error<CcmReconcileError>, Arc<Context>) -> Action`, both passed to `Controller::run` in Task 4.

- [ ] **Step 1: Write the failing tests**

Create `src/ccm_reconciler.rs` containing only this test module, and add `pub mod ccm_reconciler;` to `src/lib.rs` (before `pub mod cache_reconciler;`).

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_controller_manager::{CloudProvider, OpenstackSpec, SecretNameRef};

    fn spec_with(platform_kind: PlatformKind) -> CloudControllerManagerSpec {
        CloudControllerManagerSpec {
            platform_kind,
            provider: CloudProvider::Openstack,
            openstack: OpenstackSpec {
                chart_version: "2.36.5".to_string(),
                cloud_config_secret_ref: SecretNameRef {
                    name: "cloud-config".to_string(),
                },
                ..Default::default()
            },
        }
    }

    #[test]
    fn accepts_talos_linux_openstack_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_managers_not_named_default() {
        for name in ["second", "Default", ""] {
            let err = validate(name, &spec_with(PlatformKind::TalosLinux))
                .expect_err("non-singleton names should be rejected");

            assert!(matches!(&err, ValidationError::UnsupportedName(n) if n == name));
            assert_eq!(err.reason(), "Unsupported");
        }
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack.chart_version = String::new();
        let err = validate("default", &spec).expect_err("blank chartVersion is invalid");
        assert_eq!(err.reason(), "InvalidChartVersion");

        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack.cloud_config_secret_ref.name = "Cloud_Config".to_string();
        let err = validate("default", &spec).expect_err("bad Secret name is invalid");
        assert_eq!(err.reason(), "InvalidSecretRef");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CcmReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CcmReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CcmReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "DaemonSet".to_string(),
            name: "openstack-cloud-controller-manager".to_string(),
            timeout: std::time::Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        // Validation already writes its own Failed status; a standby must never
        // write status at all.
        let validation = CcmReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CcmReconcileError::NotLeader.failure_reason(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib ccm_reconciler 2>&1 | tail -15`
Expected: FAIL to compile: ``cannot find function `validate` in this scope``.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `src/ccm_reconciler.rs`. It follows `src/cache_reconciler.rs` step for step; the differences are the types, the chart, and no synthesized namespace (the ledger holds only rendered objects).

```rust
use crate::cloud_controller_manager::{
    CcmSpecError, CloudControllerManager, CloudControllerManagerSpec, CloudControllerManagerStatus,
};
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "CloudControllerManager {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] CcmSpecError),
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

/// There is no provider check: `CloudProvider` has one variant and serde already
/// rejects any other value. Add one alongside the second provider.
pub fn validate(name: &str, spec: &CloudControllerManagerSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::cloud_controller_manager::validate_openstack(&spec.openstack)?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum CcmReconcileError {
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

impl CcmReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CcmReconcileError::Helm(_) => Some("RenderFailed"),
            CcmReconcileError::Manifest(_) => Some("InvalidManifest"),
            CcmReconcileError::Apply(_) => Some("ApplyFailed"),
            CcmReconcileError::Validation(_)
            | CcmReconcileError::Status(_)
            | CcmReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
) -> Result<Action, CcmReconcileError> {
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
    obj: &CloudControllerManager,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CloudControllerManager> = kube::Api::all(ctx.client.clone());
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
        &obj.spec.openstack.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(ccm = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CcmReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CloudControllerManager> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.openstack.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(ccm = %name, error = %err, "validation failed");
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
        return Err(CcmReconcileError::Validation(err));
    }
    tracing::info!(
        ccm = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::cloud_controller_manager::build_values(&obj.spec.openstack);
    let rendered =
        crate::helm::render_chart(&crate::helm::OPENSTACK_CCM_CHART, &chart_version, &values)
            .await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::CCM_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered openstack cloud-controller-manager chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    // Everything this reconcile will apply is known now. Persist it before the
    // first apply so a failure, a crash or a leader change can never leave an
    // applied object out of the ledger cleanup acts on. Steady-state resyncs
    // add nothing, so they write nothing. No namespace is synthesized:
    // kube-system already exists and is not ours to delete.
    let desired: Vec<AppliedResourceRef> =
        objects.iter().map(crate::apply::resource_ref).collect();
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
    for object in &objects {
        // The chart renders neither CRDs nor custom resources today; these guard a
        // future chart bump that adds either.
        if crate::manifests::is_custom_resource(object) {
            wait_for_object_kind(&ctx.client, object).await?;
        }
        let reference =
            crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        if reference.kind == "CustomResourceDefinition" {
            crate::apply::wait_for_crd_established(
                &ctx.client,
                &reference.name,
                Duration::from_secs(10),
            )
            .await?;
        }
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
    tracing::info!(ccm = %name, phase = ?Phase::Ready, "updating status");
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
    _obj: Arc<CloudControllerManager>,
    _err: &kube::runtime::finalizer::Error<CcmReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    api: &kube::Api<CloudControllerManager>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CcmReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CloudControllerManagerStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CcmReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
) -> Result<Action, CcmReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CcmReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(ccm = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order. The CCM owns no resources that need a bounded removal
    // wait. Deleting it does not undo node initialization: providerIDs and node
    // addresses stay, and existing cloud load balancers are not deleted.
    for reference in applied.iter().rev() {
        tracing::info!(
            ccm = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(ccm = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CcmReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all; see
    // `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CloudControllerManager> = kube::Api::all(ctx.client.clone());
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

Run: `cargo test --lib ccm_reconciler 2>&1 | tail -15`
Expected: PASS, 7 tests. Then `cargo build 2>&1 | tail -5`: builds (warnings for the not-yet-used public functions do not occur because they are `pub`).

- [ ] **Step 5: Commit**

```bash
git add src/ccm_reconciler.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: CloudControllerManager reconcile, cleanup and finalizer wiring

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Register the CRD and run the third controller

**Files:**
- Modify: `src/crds.rs`, `src/main.rs`, `tests/bootstrap_manifests.rs`
- Regenerate: `deploy/crd.yaml`
- Test: `tests/bootstrap_manifests.rs`, inline tests in `src/main.rs`

**Interfaces:**
- Consumes: `CloudControllerManager` (Task 1); `ccm_reconciler::{reconcile_with_finalizer, error_policy}` (Task 3).
- Produces: `crds::generated_yaml()` now emits three CRDs in the order CniInstallation, PullThroughCache, CloudControllerManager; a running third `Controller`.

- [ ] **Step 1: Update the CRD tests so they fail**

In `tests/bootstrap_manifests.rs`, replace `crd_yaml_defines_both_platform_resources` with:

```rust
#[test]
fn crd_yaml_defines_all_platform_resources() {
    let content = std::fs::read_to_string("deploy/crd.yaml")
        .expect("deploy/crd.yaml should exist; run `cargo run -q --bin crdgen > deploy/crd.yaml`");
    let objects = parse_manifests(&content).expect("crd.yaml should be valid YAML");

    let names: Vec<String> = objects
        .iter()
        .map(|o| {
            assert_eq!(o.types.as_ref().unwrap().kind, "CustomResourceDefinition");
            o.metadata.name.clone().unwrap()
        })
        .collect();

    assert_eq!(
        names,
        vec![
            "cniinstallations.platform.rye.ninja",
            "pullthroughcaches.platform.rye.ninja",
            "cloudcontrollermanagers.platform.rye.ninja",
        ]
    );
}
```

In `src/main.rs`'s `tests` module, add:

```rust
    #[test]
    fn deletion_requested_works_for_the_cloud_controller_manager_kind_too() {
        let ccm: CloudControllerManager = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CloudControllerManager",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-26T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "openstack",
                "openstack": {
                    "chartVersion": "2.36.5",
                    "cloudConfigSecretRef": { "name": "cloud-config" }
                }
            }
        }))
        .expect("ccm should deserialize");

        assert_eq!(deletion_requested(&ccm), Some(1));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -20`
Expected: FAIL: `crd_yaml_defines_all_platform_resources` (two names, not three) and `crd_yaml_matches_the_generated_crds` may still pass. `cargo test --bin platform-controller 2>&1 | tail` FAILS to compile: `CloudControllerManager` not in scope.

- [ ] **Step 3: Register the CRD and regenerate the file**

In `src/crds.rs`, add the third entry:

```rust
    [
        crate::crd::CniInstallation::crd(),
        crate::pull_through_cache::PullThroughCache::crd(),
        crate::cloud_controller_manager::CloudControllerManager::crd(),
    ]
```

Regenerate: `cargo run -q --bin crdgen > deploy/crd.yaml`

- [ ] **Step 4: Run the third controller**

In `src/main.rs`, add to the imports:

```rust
use platform_controller::ccm_reconciler;
use platform_controller::cloud_controller_manager::CloudControllerManager;
```

Change the cache controller to clone the shared context (it currently moves it):

```rust
            cache_reconciler::error_policy,
            context.clone(),
```

Add, after the `cache_controller` definition and before `let mut sigterm`:

```rust
    // The cloud controller manager gets its own watcher, store and Controller
    // too, with the same predicate filter and the same Context (one leader lease).
    let ccm_api: Api<CloudControllerManager> = Api::all(client.clone());
    let (ccm_reader, ccm_writer) = reflector::store();
    let managers = watcher(ccm_api, watcher::Config::default())
        .default_backoff()
        .reflect(ccm_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let ccm_controller = Controller::for_stream(managers, ccm_reader)
        .run(
            ccm_reconciler::reconcile_with_finalizer,
            ccm_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled cloud controller manager"),
                Err(err) => tracing::error!(error = %err, "cloud controller manager reconcile failed"),
            }
        });
```

Add a branch to the `select!`, after `_ = cache_controller => {}`:

```rust
        _ = ccm_controller => {}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -10`
Expected: PASS (4 tests, including `crd_yaml_defines_all_platform_resources` and `crd_yaml_matches_the_generated_crds`).
Run: `cargo test --bin platform-controller 2>&1 | tail -10`
Expected: PASS (4 tests).
Run: `cargo build 2>&1 | tail -5`
Expected: builds.

- [ ] **Step 6: Commit**

```bash
git add src/crds.rs src/main.rs tests/bootstrap_manifests.rs deploy/crd.yaml
git commit -m "$(cat <<'EOF'
feat: register the CloudControllerManager CRD and run its controller

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Deploy artifacts and the example

**Files:**
- Modify: `deploy/bootstrap.yaml`, `deploy/README.md`, `tests/bootstrap_manifests.rs`
- Create: `examples/cloud-controller-manager.yaml`, `tests/cloud_controller_manager_example.rs`
- Test: `tests/bootstrap_manifests.rs`, `tests/cloud_controller_manager_example.rs`

**Interfaces:**
- Consumes: `ccm_reconciler::validate`, `cloud_controller_manager::{build_values, CloudControllerManager}` (Tasks 1, 3).
- Produces: nothing later tasks call; the example is referenced by the runbook (Task 6) as `examples/cloud-controller-manager.yaml`.

- [ ] **Step 1: Write the failing tests**

Add to `tests/bootstrap_manifests.rs`:

```rust
#[test]
fn controller_deployment_tolerates_the_uninitialized_cloud_provider_taint() {
    // With kubelets on --cloud-provider=external every node starts tainted
    // node.cloudprovider.kubernetes.io/uninitialized until the CCM initializes
    // it. This controller installs the CCM, so it has to schedule first.
    let content = std::fs::read_to_string("deploy/bootstrap.yaml").expect("bootstrap.yaml should exist");
    let objects = parse_manifests(&content).expect("bootstrap.yaml should be valid YAML documents");
    let deployment = objects
        .iter()
        .find(|o| o.types.as_ref().unwrap().kind == "Deployment")
        .expect("bootstrap.yaml has the controller Deployment");

    let tolerations = deployment
        .data
        .pointer("/spec/template/spec/tolerations")
        .and_then(|value| value.as_array())
        .expect("the Deployment has tolerations");

    assert!(
        tolerations.iter().any(|toleration| {
            toleration["key"] == "node.cloudprovider.kubernetes.io/uninitialized"
                && toleration["operator"] == "Exists"
                && toleration["effect"] == "NoSchedule"
        }),
        "{tolerations:?}"
    );
}
```

Create `tests/cloud_controller_manager_example.rs`:

```rust
use platform_controller::ccm_reconciler;
use platform_controller::cloud_controller_manager::{build_values, CloudControllerManager};
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/cloud-controller-manager.yaml";

fn load() -> CloudControllerManager {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a CloudControllerManager")
}

#[test]
fn example_is_a_single_cloud_controller_manager_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CloudControllerManager");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let manager = load();

    assert_eq!(manager.metadata.name.as_deref(), Some("default"));
    ccm_reconciler::validate("default", &manager.spec).expect("the example must be valid");
}

#[test]
fn example_chart_version_is_the_chart_version_not_the_app_version() {
    // The chart is versioned 2.x; the application it deploys is v1.x. Using the
    // app version (or a `v` prefix) fails with "chart ... not found".
    let version = load().spec.openstack.chart_version;

    assert!(!version.starts_with('v'), "{version}");
    assert!(version.starts_with("2."), "{version}");
}

#[test]
fn example_values_name_the_secret_the_comment_tells_you_to_create() {
    let openstack = load().spec.openstack;
    let values = build_values(&openstack);

    assert_eq!(openstack.cloud_config_secret_ref.name, "cloud-config");
    assert_eq!(values["secret"]["name"], "cloud-config");
    assert_eq!(values["secret"]["create"], false);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test bootstrap_manifests --test cloud_controller_manager_example 2>&1 | tail -20`
Expected: FAIL: the toleration test (`[...]` printed without the key) and the example tests (`the example should exist`).

- [ ] **Step 3: Add the toleration**

In `deploy/bootstrap.yaml`, replace

```yaml
        - key: node-role.kubernetes.io/control-plane
          operator: Exists
          effect: NoSchedule
      # Preferred, not required: a small dev/test cluster may only have one or
```

with

```yaml
        - key: node-role.kubernetes.io/control-plane
          operator: Exists
          effect: NoSchedule
        # With kubelets on --cloud-provider=external (the prerequisite for a
        # CloudControllerManager), every node is tainted uninitialized until the
        # CCM runs. This controller is what installs the CCM, so it must be
        # schedulable before that.
        - key: node.cloudprovider.kubernetes.io/uninitialized
          operator: Exists
          effect: NoSchedule
      # Preferred, not required: a small dev/test cluster may only have one or
```

- [ ] **Step 4: Add the example**

Create `examples/cloud-controller-manager.yaml`:

```yaml
# Sample CloudControllerManager for a self-hosted Talos Linux cluster running on
# OpenStack VMs: the OpenStack cloud controller manager initializes nodes (sets
# providerID and addresses), removes nodes whose VM is gone, and provisions
# LoadBalancer Services. It does nothing on a managed cluster (EKS, GKE, AKS,
# OKE), where the provider already runs its own.
#
# BEFORE applying this:
#
# 1. Every node needs a one-time Talos machine-config change the controller
#    cannot make for you (see docs/runbooks/cloud-controller-manager-verification.md,
#    step 0):
#
#      cluster:
#        externalCloudProvider:
#          enabled: true
#
# 2. Create the cloud config Secret in kube-system. The controller never reads it;
#    it only tells the chart its name. The key MUST be `cloud.conf`:
#
#      kubectl -n kube-system create secret generic cloud-config \
#        --from-file=cloud.conf=./cloud.conf
#
#    cloud.conf is the OpenStack cloud config (auth-url, an application credential,
#    region, and any [Networking]/[LoadBalancer] settings); see
#    https://github.com/kubernetes/cloud-provider-openstack
#
# Apply this before the CniInstallation: the CCM runs on the host network and does
# not need the CNI, but nodes stay tainted until it has initialized them.
#
#   kubectl apply -f examples/cloud-controller-manager.yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CloudControllerManager
metadata:
  name: default
spec:
  platformKind: talos-linux
  provider: openstack
  openstack:
    # The Helm CHART version of openstack-cloud-controller-manager (2.x), not the
    # application version (v1.x), and with no "v" prefix. If .status shows
    # Failed / RenderFailed, this is the usual cause.
    chartVersion: "2.36.5"
    cloudConfigSecretRef:
      name: cloud-config
```

- [ ] **Step 5: Update the deploy README**

In `deploy/README.md`, change the apply block to wait on the third CRD and note the order:

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/cloudcontrollermanagers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cni-installation.yaml
```

Change "`crd.yaml` (both CRDs)" to "`crd.yaml` (all three CRDs)". Then add this section after the "Pull-through image cache (optional)" section:

```markdown
## Cloud controller manager (optional)

`examples/cloud-controller-manager.yaml` is a `CloudControllerManager` that
installs the OpenStack cloud controller manager on a self-hosted Talos cluster
running on OpenStack VMs. Skip it on a managed cluster (EKS, GKE, AKS, OKE): the
provider already runs one. It needs two things the controller cannot do for you:

- a one-time Talos machine-config change so kubelets use
  `--cloud-provider=external` (`docs/runbooks/cloud-controller-manager-verification.md`,
  step 0), and
- a Secret named as `spec.openstack.cloudConfigSecretRef.name` in `kube-system`,
  holding the OpenStack cloud config under the key `cloud.conf`. The controller
  never reads it, and does not check that it exists: with the Secret missing the
  DaemonSet's pod sits in `ContainerCreating` while `.status.phase` still says
  `Ready` (which means "manifests applied").

`spec.openstack.chartVersion` is the Helm chart version (`2.36.5`), not the
application version (`v1.36.0`).

**Apply order:** `CloudControllerManager`, then `CniInstallation`, then
`PullThroughCache`. The controller enforces no ordering; each reconciles when
applied. With external cloud-provider kubelets every node is tainted
`node.cloudprovider.kubernetes.io/uninitialized` until the CCM initializes it, and
the CCM runs on the host network, so it does not need the CNI.

**Upgrading an existing install:** apply the new `deploy/crd.yaml` (and wait for
all three CRDs to be Established) *before* rolling the controller image. The new
image's Deployment also tolerates the `uninitialized` taint (`deploy/bootstrap.yaml`);
without that toleration the controller could not schedule on a cluster whose
kubelets use an external cloud provider.

**Deleting** a `CloudControllerManager` removes the chart's objects but does not
undo node initialization (providerIDs and addresses stay) and does not delete
existing cloud load balancers.
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test bootstrap_manifests --test cloud_controller_manager_example 2>&1 | tail -20`
Expected: PASS (5 + 4 tests).

- [ ] **Step 7: Commit**

```bash
git add deploy/bootstrap.yaml deploy/README.md examples/cloud-controller-manager.yaml tests/bootstrap_manifests.rs tests/cloud_controller_manager_example.rs
git commit -m "$(cat <<'EOF'
feat: tolerate the uninitialized taint and add the CloudControllerManager example

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Integration test, runbook and memory

**Files:**
- Create: `tests/integration_cloud_controller_manager.rs`, `docs/runbooks/cloud-controller-manager-verification.md`, `docs/memory/cloud-controller-manager-2026-09.md`
- Modify: `docs/memory/MEMORY.md`, `docs/memory/rbac-cluster-admin-tradeoff.md`
- Test: the ignored integration test compiles (`cargo test --no-run`)

**Interfaces:**
- Consumes: `CloudControllerManager` (Task 1); `examples/cloud-controller-manager.yaml` (Task 5).
- Produces: nothing later tasks use.

- [ ] **Step 1: Write the ignored integration test**

Create `tests/integration_cloud_controller_manager.rs`:

```rust
// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running and all three CRDs Established:
//
//   kubectl apply -f examples/cloud-controller-manager.yaml
//   cargo test --test integration_cloud_controller_manager -- --ignored --nocapture
//
// The test deletes the CloudControllerManager at the end, so re-apply the example
// to run it again. It does NOT assert that the CCM pod runs: without the
// cloud-config Secret and an OpenStack to talk to, the pod cannot start. That is
// a manual runbook step (docs/runbooks/cloud-controller-manager-verification.md).

use k8s_openapi::api::apps::v1::DaemonSet;
use kube::api::{Api, DeleteParams};
use kube::Client;
use platform_controller::cloud_controller_manager::CloudControllerManager;
use platform_controller::crd::Phase;
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
async fn openstack_ccm_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let managers: Api<CloudControllerManager> = Api::all(client.clone());
    let daemon_sets: Api<DaemonSet> = Api::namespaced(client.clone(), "kube-system");

    eventually("CloudControllerManager reaching Ready", Duration::from_secs(300), || async {
        managers
            .get("default")
            .await
            .ok()
            .and_then(|manager| manager.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    assert!(
        daemon_sets
            .get_opt("openstack-cloud-controller-manager")
            .await
            .expect("get_opt should succeed")
            .is_some(),
        "Ready but the DaemonSet does not exist in kube-system"
    );

    managers
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the CloudControllerManager");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        managers.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the DaemonSet disappearing", Duration::from_secs(120), || async {
        daemon_sets
            .get_opt("openstack-cloud-controller-manager")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo test --test integration_cloud_controller_manager --no-run 2>&1 | tail -5`
Expected: compiles. Run `cargo test --test integration_cloud_controller_manager 2>&1 | tail -5`: `1 ignored`.

- [ ] **Step 3: Write the runbook**

Create `docs/runbooks/cloud-controller-manager-verification.md`:

````markdown
# Verifying the OpenStack cloud controller manager on Talos

Manual acceptance for the `CloudControllerManager` resource. Needs a real
OpenStack cloud you can boot Talos VMs in, credentials for it, and at least one
control-plane and one worker node. Nothing here has been run yet: record what you
observe under "Findings to record" at the end, the way
`pull-through-cache-verification.md` does.

## 0. Node prerequisite (once, at cluster creation)

Every node's kubelet must run with `--cloud-provider=external`, and the control-plane
components must be configured for an external cloud provider. Talos exposes this as
a machine-config switch. Patch the machine config of every node (control plane and
workers) at creation:

```yaml
# ccm-talos-patch.yaml
cluster:
  externalCloudProvider:
    enabled: true
```

```sh
talosctl gen config <cluster> https://<endpoint>:6443 --config-patch @ccm-talos-patch.yaml
```

On a running cluster use `talosctl patch machineconfig --nodes <ip> --patch @ccm-talos-patch.yaml`
instead; kubelet needs a restart, which Talos reports. Confirm the result on a node:

```sh
talosctl -n <node-ip> get kubeletspecs -o yaml | grep -i cloud-provider    # external
```

Record the Talos version and the exact patch that worked. The controller cannot
apply or check this; it reports only that its own manifests were applied.

From now on each node registers with the taint
`node.cloudprovider.kubernetes.io/uninitialized:NoSchedule` and keeps it until the
CCM initializes it:

```sh
kubectl get nodes -o custom-columns=NAME:.metadata.name,TAINTS:.spec.taints[*].key
```

## 1. The controller schedules under the new toleration

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/cloudcontrollermanagers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl -n platform-system get pods -o wide
```

Expected: both `platform-controller` pods are scheduled and Running on nodes that
still carry the `uninitialized` taint. Before the toleration in `deploy/bootstrap.yaml`
they would sit `Pending` (`untolerated taint`). Record which nodes they landed on.

## 2. Create the credentials Secret, then apply

Create the cloud config as `cloud.conf` (the key matters). A minimal example; use
your own auth URL, an application credential and region, and add `[Networking]` /
`[LoadBalancer]` sections as needed (see the cloud-provider-openstack docs):

```ini
[Global]
auth-url=https://keystone.example.com:5000/v3
application-credential-id=<id>
application-credential-secret=<secret>
region=RegionOne

[LoadBalancer]
use-octavia=true
floating-network-id=<external-network-uuid>
subnet-id=<subnet-uuid>
```

```sh
kubectl -n kube-system create secret generic cloud-config --from-file=cloud.conf=./cloud.conf
kubectl apply -f examples/cloud-controller-manager.yaml
kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n kube-system get ds openstack-cloud-controller-manager -o wide
kubectl -n kube-system get pods -l app=openstack-cloud-controller-manager -o wide
```

Expected: `Ready`; one pod per control-plane node, Running. `Ready` means the
manifests were applied, not that the CCM is healthy: the pod is the real signal.
Check `kubectl -n kube-system logs <pod>` for errors reaching Keystone.

This also verifies the `extraVolumes: []` override: the pod starts on Talos with
no hostPath mounts (`kubectl -n kube-system get pod <pod> -o yaml | grep -c hostPath` is `0`).

## 3. Node initialization

```sh
kubectl get nodes -o custom-columns=NAME:.metadata.name,PROVIDERID:.spec.providerID,TAINTS:.spec.taints[*].key
```

Expected: every node has a `providerID` of the form `openstack:///<instance-uuid>` and
no `uninitialized` taint. Record the time from applying the CR to the taint clearing.

## 4. Node addresses and the IPv6 interaction

```sh
kubectl get nodes -o wide
kubectl get node <node> -o jsonpath='{.status.addresses}{"\n"}'
```

The CCM can rewrite each node's addresses from Neutron. Compare the `InternalIP` /
`ExternalIP` values before and after step 2 (`kubectl get nodes -o wide` beforehand).
If the cluster uses `nodeAddressAutodetectionV6Method: kubernetesInternalIP` on
`CniInstallation`, check whether Calico's view of node addresses changed
(`kubectl get nodes.projectcalico.org -o yaml` or the `calico-node` logs) and record
what you find. Nothing is designed around this yet.

## 5. LoadBalancer Service

```sh
kubectl create deployment web --image=registry.k8s.io/pause:3.10 --replicas=1
kubectl expose deployment web --port=80 --type=LoadBalancer
kubectl get svc web -w         # EXTERNAL-IP moves from <pending> to an address
```

Expected: an external address, and a matching load balancer in Octavia. Delete the
Service afterwards and confirm the load balancer is removed
(`openstack loadbalancer list`).

## 6. The `route` controller under Calico

The chart enables the `route` controller by default. Look for it doing anything to
Neutron routes or fighting Calico:

```sh
kubectl -n kube-system logs -l app=openstack-cloud-controller-manager | grep -i route
openstack router show <router> -c routes
```

If it programs routes that conflict with Calico's pod routing, record it: the fix is an
unconditional `enabledControllers` override without `route` (spec: "The `route`
controller"), and the spec and `build_values` change with it.

## 7. Missing Secret

```sh
kubectl -n kube-system delete secret cloud-config
kubectl -n kube-system delete pod -l app=openstack-cloud-controller-manager
kubectl -n kube-system get pods -l app=openstack-cloud-controller-manager     # ContainerCreating
kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}'                   # still Ready
```

Expected (and by design): the pod cannot mount the Secret and stays
`ContainerCreating`, while status still says `Ready`. Recreate the Secret and the
pod starts. This is the failure mode to check first when nodes stay tainted.

## 8. Delete, and what stays

```sh
kubectl delete ccm default        # returns once the finalizer clears
kubectl -n kube-system get ds openstack-cloud-controller-manager          # NotFound
kubectl get nodes -o custom-columns=NAME:.metadata.name,PROVIDERID:.spec.providerID
```

Expected: the chart's objects are gone; `providerID`s remain (node initialization is
not undone). `kube-system` is untouched. Existing cloud load balancers remain, and
`LoadBalancer` Services stop being reconciled until the CR is re-applied.

## When something goes wrong

Failures appear on the resource, not only in the controller's logs:

- `kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidSecretRef`,
  `InvalidHelmValues`, `Unsupported`; `RenderFailed` when helm cannot render the
  chart, e.g. the app version `v1.36.0` used as the chart version; `InvalidManifest`;
  `ApplyFailed` when the API server rejects an object) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is applied, so
  deleting the resource after a failed first install still removes everything that
  was created.

## Findings to record

To fill in from the first live run: the Talos version and patch that worked (step 0),
where the controller pods scheduled (step 1), the time to node initialization (step
3), the address comparison and Calico's behavior (step 4), the LoadBalancer result
(step 5), what the `route` controller did (step 6), and anything in step 8 that
differs from "Expected".
````

- [ ] **Step 4: Write the memory entry and index line**

Create `docs/memory/cloud-controller-manager-2026-09.md`:

```markdown
---
name: cloud-controller-manager-2026-09
description: CloudControllerManager (OpenStack) slice, 2026-09-26 - third CRD beside CniInstallation and PullThroughCache; chart facts from a real render; nothing live-verified yet
metadata:
  type: project
---

`CloudControllerManager` (cluster-scoped singleton `default`, shortname `ccm`) installs cloud-provider-openstack on self-hosted Talos clusters running on OpenStack VMs. Spec: `docs/superpowers/specs/2026-09-26-cloud-controller-manager-design.md`; plan: `docs/superpowers/plans/2026-09-26-cloud-controller-manager.md`; live acceptance: `docs/runbooks/cloud-controller-manager-verification.md`. It is a third parallel component (own reconciler, third `Controller` in `main.rs`, same leader lease). The finalizer and status glue is now duplicated three times: that is the evidence for extracting a shared component framework, deliberately not done in this slice ([[pull-through-cache-2026-09]] deferred it until a third component). AWS, GCP, Azure and OCI providers, Cinder/Manila CSI, typed `cloud.conf` fields and a Secret existence check are deliberately not built.

**Non-obvious facts (from rendering the real chart, not assumed):**
- Chart `openstack-cloud-controller-manager` `2.36.5` (app `v1.36.0`) from the classic repo `https://kubernetes.github.io/cloud-provider-openstack`; `chartVersion` is the CHART version (2.x), not the app version, and has no `v` prefix. The existing `--repo` render path is reused unchanged.
- `secret.create=false` renders NO Secret object. The DaemonSet mounts the Secret by name and reads `/etc/config/cloud.conf`, so the key inside the user's Secret must be `cloud.conf`; the chart's `secret-reader` Role is scoped by `resourceNames` to the same name.
- The chart's default `extraVolumes` hostPath-mount `/etc/kubernetes/pki` and the flexvolume dir; the controller always overrides both lists with `[]` (Talos provides neither, the CCM uses in-cluster config). With the override the only volume is the Secret.
- The DaemonSet is `hostNetwork` and tolerates `node.cloudprovider.kubernetes.io/uninitialized`, so it needs no CNI. It targets `node-role.kubernetes.io/control-plane` nodes and lives in `kube-system` (no namespace is synthesized; Talos exempts it from Pod Security).
- The release name `openstack-ccm` is in the DaemonSet's immutable selector (`release: openstack-ccm`); never change it.
- **The controller's own Deployment had to gain a toleration for the `uninitialized` taint** (`deploy/bootstrap.yaml`): on a cluster whose kubelets use `--cloud-provider=external` it could otherwise never schedule, and nothing could install the CCM.
- The Talos node prerequisite (`cluster.externalCloudProvider.enabled: true`) cannot be applied or checked by the controller; `Ready` means manifests applied only. A missing cloud-config Secret shows as `Ready` with a pod stuck in `ContainerCreating`.

**Not verified:** everything on a live cluster. Open items recorded in the runbook: node initialization, node addresses (and the interaction with `nodeAddressAutodetectionV6Method: kubernetesInternalIP`), LoadBalancer Services, whether the chart-default `route` controller conflicts with Calico (if so, override `enabledControllers` without `route`), the controller pod scheduling under the new toleration, and the `extraVolumes: []` override on Talos.

**How to apply:** when bumping the chart version, re-run the ignored real-chart test `helm::tests::openstack_ccm_chart_renders_the_shape_the_spec_relies_on` and re-check the Secret mount, the volumes and the hook rendering.
```

In `docs/memory/MEMORY.md`, append one line:

```markdown
- [CloudControllerManager (OpenStack) slice](cloud-controller-manager-2026-09.md) — third CRD; chart 2.36.5 facts (no Secret rendered, key cloud.conf, hostPath volumes overridden); controller Deployment needs the uninitialized-taint toleration; nothing live-verified yet
```

- [ ] **Step 5: Add the RBAC ledger rows**

In `docs/memory/rbac-cluster-admin-tradeoff.md`, add these rows directly after the row that begins `| \`""\` (core), \`apps\` | \`serviceaccounts\`, \`services\`, \`daemonsets\`, \`namespaces\` | Spegel chart`:

```markdown
| `platform.rye.ninja` | `cloudcontrollermanagers`, `cloudcontrollermanagers/status` | Controller itself | Its own third CRD (2026-09-26) — get/list/watch/update/patch, same as `cniinstallations`; finalizer updates go through `update`/`patch` on the main resource. |
| `""` (core), `apps`, `rbac.authorization.k8s.io` | `serviceaccounts`, `daemonsets` in `kube-system`; `clusterroles`, `clusterrolebindings`, `roles`, `rolebindings` | OpenStack CCM chart (`2.36.5`, rendered with `--no-hooks`) | Applies no kinds the controller does not already apply. The chart's ClusterRole grants `nodes` get/list/watch/patch/update, `nodes/status` patch, `services` list/patch/update/watch, `services/status` patch, `serviceaccounts` create/get, `serviceaccounts/token` create, `endpoints` create/get/list/watch/update, `configmaps` get/list/watch, `events` create/patch/update, `leases` get/create/update; because of privilege-escalation prevention the controller must hold every one of those to create it (currently guaranteed by `cluster-admin`). A `Role` in `kube-system` grants `secrets` get/list/watch on the cloud-config Secret by `resourceNames`. The controller itself never reads that Secret. |
```

- [ ] **Step 6: Commit**

```bash
git add tests/integration_cloud_controller_manager.rs docs/runbooks/cloud-controller-manager-verification.md docs/memory/cloud-controller-manager-2026-09.md docs/memory/MEMORY.md docs/memory/rbac-cluster-admin-tradeoff.md
git commit -m "$(cat <<'EOF'
docs: CloudControllerManager runbook, integration test and memory

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7: Full verification**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | grep -E "^test result|FAILED|failed" ; cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | sort | uniq -c`
Expected: build succeeds; every `test result` line is `ok` with 0 failed (ignored tests are skipped); clippy reports only warning classes the existing code already triggers (`result_large_err`, `too_many_arguments`, and the pre-existing ones in `apply.rs` and `manifests.rs`), no errors.

Then run the real-chart tests, which need network and `helm`: `cargo test --lib -- --ignored openstack_ccm 2>&1 | tail -8`. Expected: PASS.

---

## Self-Review

**Spec coverage.**
- API and validation (chartVersion empty/whitespace, Secret name, helmValues object, name `default`, `platformKind`, provider via serde): Task 1 and Task 3.
- Values layering, Secret passthrough, Talos volume overrides, no `clusterName`/`enabledControllers`/`cloudConfig` typing: Task 1.
- Reconcile flow, no synthesized namespace, ledger checkpoint, prune, `Ready`, 300s requeue: Task 3.
- Missing Secret is not checked and is documented: Task 5 (README, example) and Task 6 (runbook step 7).
- `route` controller left at the chart default and treated as a live item: Task 6 (runbook step 6) and the memory entry.
- Bootstrap toleration and apply order: Task 5.
- Cleanup semantics (reverse ledger order, no waits, node init not undone): Task 3 (code comments), Task 5 (README), Task 6 (runbook step 8).
- IPv6 node-address interaction as an open item: Task 6 (runbook step 4).
- Wiring (third watcher and controller, same lease, predicate filter): Task 4.
- Code layout, CRD file regeneration, README, example, runbook, memory, RBAC ledger rows: Tasks 4 to 6.
- Testing (unit, real-chart ignored, example, integration ignored, live runbook): Tasks 1 to 3, 5 and 6.
- Confirming the existing `--repo` render path needs no change: Task 2 (test) and no `helm.rs` logic change.

**Placeholder scan.** None. The runbook's "Findings to record" section lists what to capture from a run that has not happened; it is not a deferred implementation step.

**Type consistency.** `OpenstackSpec`, `SecretNameRef`, `CloudProvider`, `CloudControllerManagerSpec/Status`, `CcmSpecError` (variants `EmptyChartVersion`, `ChartVersionHasWhitespace`, `InvalidSecretName`, `HelmValuesNotObject`), `validate_openstack`, `build_values` (Task 1) match their uses in Tasks 2 to 5. `OPENSTACK_CCM_CHART` and `CCM_NAMESPACE` (Task 2) match Task 3. `ValidationError`, `CcmReconcileError`, `validate`, `reconcile_with_finalizer`, `error_policy` (Task 3) match Tasks 4 and 5. The DaemonSet name `openstack-cloud-controller-manager` matches the rendered chart in Tasks 2 and 6.

**Review Focus coverage.** Item 1: Task 1 `rejects_secret_names_kubernetes_rejects`. Item 2: Task 1 `typed_and_talos_values_win_over_conflicting_helm_values`. Item 3: Task 2 real-chart test. Item 4: Task 3 `rejects_managers_not_named_default`. Item 5: Task 3 `validation_and_leadership_errors_do_not_overwrite_status`.
