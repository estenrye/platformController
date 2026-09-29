# Cert-Manager Installation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a fifth platform component to the controller: a `CertManagerInstallation` custom resource that installs cert-manager on self-hosted Talos clusters, following the exact shape of `CniInstallation`/`PullThroughCache`/`CloudControllerManager`/`CsiDriver`.

**Architecture:** A new cluster-scoped singleton CRD, `CertManagerInstallation`, with its own reconciler (`src/cert_manager_reconciler.rs`) that renders the `cert-manager` Helm chart from its OCI distribution, synthesizes and applies a plain (non-privileged) `cert-manager` namespace, server-side-applies the rendered objects into it, and prunes and cleans up through the same primitives the other four reconcilers use. `main.rs` runs a fifth `Controller` under the same leader lease. No Issuer/ClusterIssuer configuration is in scope; this component only installs cert-manager itself.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, the `helm` CLI (already a runtime dependency), chart `cert-manager` `v1.16.2` from `oci://quay.io/jetstack/charts/cert-manager`.

**Spec:** [docs/superpowers/specs/2026-09-28-cert-manager-installation-design.md](../specs/2026-09-28-cert-manager-installation-design.md)

## Global Constraints

Every task's requirements implicitly include this section. Values are copied from the spec, plus facts confirmed live by rendering the real chart while writing this plan (`helm template cert-manager oci://quay.io/jetstack/charts/cert-manager --version v1.16.2 --include-crds --no-hooks --namespace cert-manager`, 2026-09-28, helm v4.2.4).

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `CertManagerInstallation`, **cluster-scoped** (no `#[kube(namespaced)]`, same as every other kind here), shortname `certmgr` (not `cm`: `kubectl` already treats that as shorthand for ConfigMaps), plural `certmanagerinstallations` (kube-derive's default pluralization), singleton named `default`.
- `platformKind` accepts only `talos-linux` (the shared `PlatformKind` enum has one variant; serde rejects anything else).
- **No `provider` field and no nested sub-spec.** Unlike every other component's CRD, the spec is flat: `{ platformKind, chartVersion, helmValues }`. This was an explicit design decision (see the spec's Non-goals) — there is no second implementation of "the thing that installs cert-manager" to model.
- `chartVersion` is the Helm chart version. Unlike the OpenStack charts, cert-manager's chart and app versions **track together** and both carry the `v` prefix (`v1.16.2`) — do not apply the CCM/CSI "no v prefix" habit here. Required, no default, passed verbatim to `helm --version`, so leading/trailing whitespace is rejected.
- `helmValues` is optional free-form passthrough. Layering: `helmValues` merged first, then the one typed value (`crds.enabled: true`) is overlaid, so it always wins.
- Chart: `oci://quay.io/jetstack/charts/cert-manager`, release `cert-manager`, namespace `cert-manager`. `render_args` already appends `--include-crds --no-hooks --namespace <ns>` for every `ChartRef`, so no new render-argument logic is needed — only a new `ChartRef` constant.
- **Live-verified facts about chart `v1.16.2`, used to write test assertions in this plan (not guessed):**
  - `crds.enabled` **defaults to `false`** — rendering without setting it produces **zero** `CustomResourceDefinition` objects. This is why `build_values` must unconditionally force `crds.enabled: true`; it is a real requirement, not defensive redundancy.
  - With `crds.enabled: true`, exactly **6** CRDs render: `certificaterequests.cert-manager.io`, `certificates.cert-manager.io`, `challenges.acme.cert-manager.io`, `clusterissuers.cert-manager.io`, `issuers.cert-manager.io`, `orders.acme.cert-manager.io`.
  - The chart renders **no `Namespace` object** and **no `Secret` object**.
  - Exactly **3** `Deployment`s: `cert-manager-cainjector`, `cert-manager` (the controller), `cert-manager-webhook`.
  - Exactly **1** `ValidatingWebhookConfiguration` (`cert-manager-webhook`) and **1** `MutatingWebhookConfiguration`.
  - Zero occurrences of `hostPath` or `hostNetwork` anywhere in the rendered output, and every container sets `runAsNonRoot: true`, a `seccompProfile`, `allowPrivilegeEscalation: false` and drops every capability — the pods are **restricted**-PSS-safe, not merely baseline-safe. This is why the synthesized namespace carries **no** `pod-security.kubernetes.io/*` labels, unlike Calico's `tigera-operator` and Spegel's `spegel` namespaces.
  - Rendering with `--no-hooks` produces zero `helm.sh/hook` occurrences (same as every other chart in this repo).
  - The OCI chart prints `Pulled:`/`Digest:` preamble lines to stdout ahead of the manifests, exactly like Spegel. `helm::render_chart` already calls `strip_oci_pull_preamble` unconditionally for every chart, so **no code change is needed** for this — it is called out here only so the ignored real-chart test's expectations make sense.
- No `cleanupTimeoutSeconds` on this CRD: cleanup is plain reverse-ledger-order deletion (same as `PullThroughCache`'s `cleanup`), since nothing this component applies needs a bounded removal wait.
- No `wait_for_object_kind` call is needed in the reconcile loop: this component applies the chart's own CRDs as data (like any other object) but never creates a `cert-manager.io` custom resource itself, so there is no same-reconcile ordering dependency on them being registered.
- The finalizer name `platform.rye.ninja/cleanup` is reused (finalizers are per object).
- **Apply order:** after `CniInstallation` is `Ready` (cert-manager's pods run on the pod network and need cluster DNS, same reasoning as `PullThroughCache`); no dependency on `CloudControllerManager` or `CsiDriver`.
- Existing CNI/cache/CCM/CSI reconcile and cleanup logic is not modified. Only `tests/bootstrap_manifests.rs`'s `crd_yaml_defines_all_platform_resources` gains a fifth name.
- CI runs `cargo build`, `cargo test`, `cargo clippy --all-targets`. Clippy warnings are not denied; `result_large_err` already fires on the existing reconcilers and will fire on this one too — accepted.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Building needs free disk (a debug build of this crate is several GB). If a build fails with `No space left on device`, free space first; do not delete anything that is not a build artifact.
- CLAUDE.md: project memory lives in `docs/memory/` (index in `docs/memory/MEMORY.md`), committed like any other change. Do not write to the out-of-repo memory path.

## Review Focus

Failure modes the spec implies but that are easiest to miss. Each has a test in the task that owns the code.

1. A second `CertManagerInstallation` with any name other than `default` must get `Failed` / `Unsupported`, not be reconciled. Test in Task 3.
2. A `helmValues` passthrough that tries to disable CRD installation (`crds.enabled: false`) or otherwise contradict the one typed value. The typed layer must always win. Test in Task 1.
3. `helmValues` that is not a JSON object (a string, array, number or `null`) must fail validation with `InvalidHelmValues`, not fail later inside `helm template` or get silently ignored. Test in Task 1.
4. A standby replica, or a validation failure, must not overwrite status with a generic failure — `Validation` already writes its own `Failed` status, and a non-leader must never write status at all. Test in Task 3.
5. A chart bump that changes the rendered shape this plan's assumptions rely on (a `Namespace` or `Secret` starts being rendered, the CRD list changes, `crds.enabled`'s default flips, a hook object appears, or the Deployment count changes). The ignored real-chart test in Task 2 pins today's shape; re-run it on every chart version bump.

---

## Branch

The spec and this plan are committed on `main`. Create the implementation branch from there:

```bash
git checkout -b cert-manager-installation
```

## Verification status of the code in this plan

All code in Tasks 1-6 was extracted from this plan into a scratch worktree of `main` (`git worktree add /tmp/cert-manager-plan-verify main --detach`) and run on 2026-09-28: `cargo build` and `cargo build --lib` both succeeded; `cargo test --lib` passed 253 tests (22 of them new, 2 new ones `#[ignore]`d) with 0 failures; `cargo test --bin platform-controller` passed all 6 (the new deletion-requested test included); `cargo test --test bootstrap_manifests` passed all 5, including `crd_yaml_defines_all_platform_resources` (five names, `certmanagerinstallations.platform.rye.ninja` last) and `crd_yaml_matches_the_generated_crds` against the regenerated `deploy/crd.yaml`; `cargo test --test cert_manager_example` passed all 4; `cargo test --test integration_cert_manager --no-run` compiled and the one test reported `ignored` as expected; the full `cargo test` workspace run showed 0 failures across every file. Both ignored real-chart tests in `helm.rs` (`cert_manager_chart_renders_the_shape_the_spec_relies_on` and `cert_manager_omitting_crds_enabled_renders_no_crds_by_default`) were also run with `--ignored` against the live `oci://quay.io/jetstack/charts/cert-manager` chart and passed — this is what produced the exact CRD names, Deployment names and `crds.enabled` default fact recorded in Global Constraints, not a guess. `cargo clippy --all-targets` reported only the two warning classes the existing code already triggers (`result_large_err` on `CertManagerReconcileError`, `too_many_arguments` on `update_status`) — no new category.

What has **not** been run: the manual live-cluster steps (the runbook, Task 6) and the ignored Talos-in-Docker integration test. If any code block here fails to compile when you apply it, treat that as a plan bug to fix, not to work around.

## File Structure

| File | Responsibility |
|---|---|
| `src/cert_manager.rs` (create) | `CertManagerInstallation` CRD types, status type, spec validation, the values builder. |
| `src/cert_manager_reconciler.rs` (create) | Reconcile, cleanup, finalizer wiring, status for `CertManagerInstallation`. |
| `src/helm.rs` (modify) | `CERT_MANAGER_NAMESPACE`, `CERT_MANAGER_CHART`; render-args test; ignored real-chart test. |
| `src/lib.rs` (modify) | Register the two new modules. |
| `src/crds.rs` (modify) | Include the fifth CRD in `generated_yaml()`. |
| `src/main.rs` (modify) | Fifth watcher and `Controller<CertManagerInstallation>`. |
| `deploy/crd.yaml` (regenerate) | All five CRDs. |
| `deploy/README.md` (modify) | Establish-wait for the fifth CRD, apply-order notes, new Cert-manager section. |
| `examples/cert-manager.yaml` (create) | Talos starting point. |
| `tests/bootstrap_manifests.rs` (modify) | Five CRD names. |
| `tests/cert_manager_example.rs` (create) | The example parses, validates and builds the expected values. |
| `tests/integration_cert_manager.rs` (create) | Ignored: apply, assert the three Deployments, delete, assert cleanup. |
| `docs/runbooks/cert-manager-verification.md` (create) | Namespace-admission check and a self-signed `ClusterIssuer`/`Certificate` smoke test. |
| `docs/memory/cert-manager-2026-09.md`, `docs/memory/MEMORY.md` | Memory entry and index line. |

---

### Task 1: CRD types, validation and values builder

**Files:**
- Create: `src/cert_manager.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` module in `src/cert_manager.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`; `crate::pull_through_cache::{merge, preserve_unknown_object}` (both already `pub(crate)`, reused unchanged by `csi_driver.rs` and `cloud_controller_manager.rs` today — no visibility change needed here).
- Produces:
  - `CertManagerInstallation` (the CRD kind), `CertManagerInstallationSpec { platform_kind: PlatformKind, chart_version: String, helm_values: Option<serde_json::Value> }`, `CertManagerInstallationStatus { phase, observed_generation: i64, chart_version: String, applied_resources: Vec<AppliedResourceRef>, conditions: Vec<Condition> }`
  - `CertManagerSpecError` with `reason(&self) -> &'static str` returning `"InvalidChartVersion"` or `"InvalidHelmValues"`
  - `validate_cert_manager(&CertManagerInstallationSpec) -> Result<(), CertManagerSpecError>`
  - `build_values(&CertManagerInstallationSpec) -> serde_json::Value`

- [ ] **Step 1: Write the failing tests**

Create `src/cert_manager.rs` containing only the test module below (the implementation follows in Step 3), and add `pub mod cert_manager;` to `src/lib.rs` after `pub mod cache_reconciler;`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn spec() -> CertManagerInstallationSpec {
        CertManagerInstallationSpec {
            platform_kind: crate::crd::PlatformKind::TalosLinux,
            chart_version: "v1.16.2".to_string(),
            helm_values: None,
        }
    }

    #[test]
    fn spec_deserializes_with_only_the_required_fields() {
        let spec: CertManagerInstallationSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "chartVersion": "v1.16.2"
        }))
        .expect("minimal spec should deserialize");

        assert_eq!(spec.platform_kind, crate::crd::PlatformKind::TalosLinux);
        assert_eq!(spec.chart_version, "v1.16.2");
        assert!(spec.helm_values.is_none());
    }

    #[test]
    fn a_spec_without_a_chart_version_is_rejected_at_deserialization() {
        let result = serde_json::from_value::<CertManagerInstallationSpec>(serde_json::json!({
            "platformKind": "talos-linux"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn accepts_a_minimal_spec() {
        assert_eq!(validate_cert_manager(&spec()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_chart_version() {
        let mut s = spec();
        s.chart_version = "  ".to_string();

        let err = validate_cert_manager(&s).expect_err("blank chartVersion is invalid");

        assert_eq!(err, CertManagerSpecError::EmptyChartVersion);
        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn rejects_chart_versions_with_leading_or_trailing_whitespace() {
        for version in [" v1.16.2", "v1.16.2 ", "v1.16.2\n"] {
            let mut s = spec();
            s.chart_version = version.to_string();

            let err = validate_cert_manager(&s).expect_err("whitespace must be rejected");

            assert_eq!(err, CertManagerSpecError::ChartVersionHasWhitespace(version.to_string()));
            assert_eq!(err.reason(), "InvalidChartVersion");
        }
    }

    #[test]
    fn rejects_helm_values_that_are_not_an_object() {
        for value in [serde_json::json!("nope"), serde_json::json!(["a"]), serde_json::json!(3), serde_json::json!(null)] {
            let mut s = spec();
            s.helm_values = Some(value.clone());

            let err = validate_cert_manager(&s).expect_err("non-object helmValues is invalid");

            assert_eq!(err, CertManagerSpecError::HelmValuesNotObject, "{value}");
            assert_eq!(err.reason(), "InvalidHelmValues");
        }
    }

    #[test]
    fn accepts_helm_values_that_are_objects() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "replicaCount": 2 }));

        assert_eq!(validate_cert_manager(&s), Ok(()));
    }

    #[test]
    fn values_always_enable_crds() {
        let values = build_values(&spec());

        assert_eq!(values["crds"]["enabled"], true);
    }

    #[test]
    fn typed_crds_enabled_always_overrides_a_helm_values_attempt_to_disable_it() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "crds": { "enabled": false } }));

        let values = build_values(&s);

        assert_eq!(values["crds"]["enabled"], true);
    }

    #[test]
    fn helm_values_pass_through_alongside_the_typed_value() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "replicaCount": 2, "webhook": { "replicaCount": 2 } }));

        let values = build_values(&s);

        assert_eq!(values["replicaCount"], 2);
        assert_eq!(values["webhook"]["replicaCount"], 2);
        assert_eq!(values["crds"]["enabled"], true);
    }

    #[test]
    fn crd_is_cluster_scoped_with_the_certmgr_shortname() {
        let crd = CertManagerInstallation::crd();

        assert_eq!(crd.metadata.name.as_deref(), Some("certmanagerinstallations.platform.rye.ninja"));
        assert_eq!(crd.spec.group, "platform.rye.ninja");
        assert_eq!(crd.spec.names.kind, "CertManagerInstallation");
        assert_eq!(crd.spec.scope, "Cluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["certmgr".to_string()]));
    }

    #[test]
    fn crd_schema_lets_helm_values_carry_arbitrary_keys() {
        let yaml = serde_yaml::to_string(&CertManagerInstallation::crd()).expect("CRD serializes");

        assert!(
            yaml.contains("x-kubernetes-preserve-unknown-fields: true"),
            "helmValues must be a structural, unknown-field-preserving object:\n{yaml}"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cert_manager:: 2>&1 | tail -40`
Expected: FAIL to compile — `CertManagerInstallationSpec`, `validate_cert_manager`, `build_values`, `CertManagerSpecError`, `CertManagerInstallation` are not defined yet.

- [ ] **Step 3: Write the implementation**

Add above the test module in `src/cert_manager.rs`:

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CertManagerInstallation",
    status = "CertManagerInstallationStatus",
    shortname = "certmgr"
)]
#[serde(rename_all = "camelCase")]
pub struct CertManagerInstallationSpec {
    pub platform_kind: PlatformKind,
    pub chart_version: String,
    /// Free-form values merged into the chart's values. The controller's own
    /// typed value (`crds.enabled: true`) is overlaid afterwards, so it always
    /// wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::pull_through_cache::preserve_unknown_object")]
    pub helm_values: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CertManagerInstallationStatus {
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
pub enum CertManagerSpecError {
    #[error("spec.chartVersion must not be empty")]
    EmptyChartVersion,
    #[error("spec.chartVersion {0:?} has leading or trailing whitespace")]
    ChartVersionHasWhitespace(String),
    #[error("spec.helmValues must be a JSON object")]
    HelmValuesNotObject,
}

impl CertManagerSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            CertManagerSpecError::EmptyChartVersion | CertManagerSpecError::ChartVersionHasWhitespace(_) => {
                "InvalidChartVersion"
            }
            CertManagerSpecError::HelmValuesNotObject => "InvalidHelmValues",
        }
    }
}

/// Rejects specs the chart would silently mis-handle, before anything is
/// applied.
pub fn validate_cert_manager(spec: &CertManagerInstallationSpec) -> Result<(), CertManagerSpecError> {
    if spec.chart_version.trim().is_empty() {
        return Err(CertManagerSpecError::EmptyChartVersion);
    }
    if spec.chart_version.trim() != spec.chart_version {
        return Err(CertManagerSpecError::ChartVersionHasWhitespace(spec.chart_version.clone()));
    }
    if let Some(values) = &spec.helm_values
        && !values.is_object()
    {
        return Err(CertManagerSpecError::HelmValuesNotObject);
    }
    Ok(())
}

/// The Helm values for the cert-manager chart: the user's `helmValues`
/// passthrough, with the one typed field overlaid on top.
///
/// `crds.enabled` defaults to `false` on this chart (live-verified against
/// v1.16.2: rendering without it produces zero CustomResourceDefinition
/// objects), so it is always forced to `true` here -- this component's whole
/// job is installing cert-manager, CRDs included, and a `helmValues` attempt
/// to disable them must not be able to silently break that.
pub fn build_values(spec: &CertManagerInstallationSpec) -> serde_json::Value {
    let mut values = spec.helm_values.clone().unwrap_or_else(|| serde_json::json!({}));

    let typed = serde_json::json!({
        "crds": {
            "enabled": true,
        },
    });

    crate::pull_through_cache::merge(&mut values, typed);
    values
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cert_manager:: 2>&1 | tail -40`
Expected: PASS, 13 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cert_manager.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add the CertManagerInstallation CRD type and values builder

New fifth platform component: CRD types, status, spec validation and
the Helm values builder for installing cert-manager. No provider enum
and no Issuer/ClusterIssuer configuration -- this component only
installs cert-manager itself (see the design spec's Non-goals).

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
- Consumes: `ChartRef`, `ChartSource`, `render_args`, `render_chart` (all already defined in `src/helm.rs`); `crate::cert_manager::{CertManagerInstallationSpec, build_values}` from Task 1.
- Produces: `CERT_MANAGER_NAMESPACE: &str`, `CERT_MANAGER_CHART: ChartRef`.

- [ ] **Step 1: Write the failing test**

In `src/helm.rs`'s `#[cfg(test)] mod tests` block, add (near `oci_render_args_pass_the_reference_without_a_repo_flag`):

```rust
#[test]
fn cert_manager_render_args_use_the_oci_form_and_the_cert_manager_namespace() {
    let path = std::path::Path::new("/tmp/values.yaml");
    let args = render_args(&CERT_MANAGER_CHART, "v1.16.2", path);

    assert_eq!(
        args,
        vec![
            "template".to_string(),
            "cert-manager".to_string(),
            "oci://quay.io/jetstack/charts/cert-manager".to_string(),
            "--version".to_string(),
            "v1.16.2".to_string(),
            "--values".to_string(),
            "/tmp/values.yaml".to_string(),
            "--include-crds".to_string(),
            "--no-hooks".to_string(),
            "--namespace".to_string(),
            "cert-manager".to_string(),
        ]
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib helm::tests::cert_manager_render_args -- --exact 2>&1 | tail -20`
Expected: FAIL to compile — `CERT_MANAGER_CHART` is not defined yet.

- [ ] **Step 3: Add the chart constants**

In `src/helm.rs`, add after the `CINDER_CSI_NAMESPACE` constant (around line 28):

```rust
/// Namespace the cert-manager chart's namespaced objects belong in. Like
/// tigera-operator and Spegel, the chart renders no `Namespace` object of its
/// own (live-verified against chart v1.16.2).
pub const CERT_MANAGER_NAMESPACE: &str = "cert-manager";
```

Add after the `OPENSTACK_CINDER_CSI_CHART` constant (around line 84):

```rust
/// The OCI registry cert-manager's chart is published to -- Jetstack's
/// current recommended distribution channel. The classic
/// `https://charts.jetstack.io` repo is deprecated.
pub const CERT_MANAGER_CHART: ChartRef = ChartRef {
    release: "cert-manager",
    source: ChartSource::Oci {
        reference: "oci://quay.io/jetstack/charts/cert-manager",
    },
    namespace: CERT_MANAGER_NAMESPACE,
};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib helm::tests::cert_manager_render_args -- --exact 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Add the ignored real-chart test**

In `src/helm.rs`'s test module, add near the other ignored real-chart tests (after `openstack_cinder_csi_chart_renders_the_shape_the_spec_relies_on`):

```rust
#[tokio::test]
#[ignore = "requires network access and the helm CLI to be installed"]
async fn cert_manager_chart_renders_the_shape_the_spec_relies_on() {
    let spec = crate::cert_manager::CertManagerInstallationSpec {
        platform_kind: crate::crd::PlatformKind::TalosLinux,
        chart_version: "v1.16.2".to_string(),
        helm_values: None,
    };
    let values = crate::cert_manager::build_values(&spec);

    let rendered = render_chart(&CERT_MANAGER_CHART, &spec.chart_version, &values)
        .await
        .expect("helm template should succeed");
    let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

    let kinds: Vec<&str> = objects
        .iter()
        .map(|o| o.types.as_ref().expect("every rendered object has a type").kind.as_str())
        .collect();

    // crds.enabled: true is always set (build_values), so the chart's own six
    // CRDs are always included; this chart renders no Namespace and no Secret.
    let crd_names: Vec<&str> = objects
        .iter()
        .filter(|o| o.types.as_ref().unwrap().kind == "CustomResourceDefinition")
        .map(|o| o.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        crd_names,
        vec![
            "certificaterequests.cert-manager.io",
            "certificates.cert-manager.io",
            "challenges.acme.cert-manager.io",
            "clusterissuers.cert-manager.io",
            "issuers.cert-manager.io",
            "orders.acme.cert-manager.io",
        ],
        "{crd_names:?}"
    );
    assert!(!kinds.contains(&"Namespace"), "{kinds:?}");
    assert!(!kinds.contains(&"Secret"), "{kinds:?}");

    // Exactly the controller, webhook and cainjector Deployments.
    let deployment_names: Vec<&str> = objects
        .iter()
        .filter(|o| o.types.as_ref().unwrap().kind == "Deployment")
        .map(|o| o.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        {
            let mut sorted = deployment_names.clone();
            sorted.sort_unstable();
            sorted
        },
        vec!["cert-manager", "cert-manager-cainjector", "cert-manager-webhook"],
        "{deployment_names:?}"
    );
    assert!(kinds.contains(&"ValidatingWebhookConfiguration"), "{kinds:?}");
    assert!(kinds.contains(&"MutatingWebhookConfiguration"), "{kinds:?}");

    // No hostPath/hostNetwork anywhere: confirms the synthesized namespace
    // needs no pod-security.kubernetes.io/* labels, unlike Calico/Spegel.
    assert!(!rendered.contains("hostPath"), "{rendered}");
    assert!(!rendered.contains("hostNetwork"), "{rendered}");

    // --no-hooks: no hook objects are rendered as live objects.
    assert!(!rendered.contains("helm.sh/hook"));
}

#[tokio::test]
#[ignore = "requires network access and the helm CLI to be installed"]
async fn cert_manager_omitting_crds_enabled_renders_no_crds_by_default() {
    // Pins the chart-default fact build_values relies on: without the typed
    // override, this chart ships no CRDs at all (unlike Calico, where CRDs
    // come from the chart in older versions and from the running operator in
    // newer ones).
    let rendered = render_chart(&CERT_MANAGER_CHART, "v1.16.2", &serde_json::json!({}))
        .await
        .expect("helm template should succeed");
    let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

    assert!(
        !objects.iter().any(|o| o.types.as_ref().unwrap().kind == "CustomResourceDefinition"),
        "chart default changed: CRDs now render without crds.enabled"
    );
}
```

- [ ] **Step 6: Run every test to verify nothing else broke**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS (the two new `#[ignore]`d tests are skipped by default; everything else, including all pre-existing tests, still passes).

- [ ] **Step 7: Commit**

```bash
git add src/helm.rs
git commit -m "$(cat <<'EOF'
feat: wire the cert-manager chart into the Helm render layer

Adds the CERT_MANAGER_NAMESPACE/CERT_MANAGER_CHART constants (OCI
distribution, same form as Spegel) and an ignored real-chart test that
pins the exact rendered shape confirmed live against v1.16.2: 6 CRDs
only when crds.enabled is forced true, no Namespace/Secret, exactly 3
Deployments, no hostPath/hostNetwork anywhere.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Reconciler

**Files:**
- Create: `src/cert_manager_reconciler.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` module in `src/cert_manager_reconciler.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`; `crate::cert_manager::{CertManagerInstallation, CertManagerInstallationSpec, CertManagerInstallationStatus, CertManagerSpecError, build_values}` (Task 1); `crate::helm::CERT_MANAGER_CHART` (Task 2); `crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME}` (all already `pub`, reused unchanged by `cache_reconciler.rs` today).
- Produces:
  - `ValidationError` with `reason(&self) -> &'static str`
  - `validate(name: &str, spec: &CertManagerInstallationSpec) -> Result<(), ValidationError>`
  - `cert_manager_namespace_object() -> kube::api::DynamicObject`
  - `CertManagerReconcileError` with `failure_reason(&self) -> Option<&'static str>`
  - `reconcile(obj: Arc<CertManagerInstallation>, ctx: Arc<Context>) -> Result<Action, CertManagerReconcileError>`
  - `cleanup(obj: Arc<CertManagerInstallation>, ctx: Arc<Context>) -> Result<Action, CertManagerReconcileError>`
  - `error_policy(...) -> Action`
  - `reconcile_with_finalizer(obj: Arc<CertManagerInstallation>, ctx: Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<CertManagerReconcileError>>`

Note: unlike `cache_reconciler`'s `spegel_namespace_object` and CNI's `tigera_operator_namespace_object`, `cert_manager_namespace_object` carries **no** `pod-security.kubernetes.io/*` labels (see Global Constraints — the chart's pods are restricted-PSS-safe). This is the one deliberate difference from the pattern those two follow; keep it that way even though it looks like a copy-paste omission at a glance.

- [ ] **Step 1: Write the failing tests**

Create `src/cert_manager_reconciler.rs` containing only the code below (implementation follows in Step 3), and add `pub mod cert_manager_reconciler;` to `src/lib.rs` after `pub mod cert_manager;`.

```rust
use crate::cert_manager::{
    CertManagerInstallation, CertManagerInstallationSpec, CertManagerInstallationStatus, CertManagerSpecError,
};
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME};
use kube::api::DynamicObject;
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(platform_kind: PlatformKind) -> CertManagerInstallationSpec {
        CertManagerInstallationSpec {
            platform_kind,
            chart_version: "v1.16.2".to_string(),
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
    fn synthesized_namespace_is_named_cert_manager_with_no_privileged_labels() {
        let object = cert_manager_namespace_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("cert-manager"));
        assert!(object.metadata.namespace.is_none());
        // Unlike Calico's tigera-operator and Spegel's spegel namespace, this
        // one carries no pod-security labels at all: the chart's Deployments
        // are restricted-PSS-safe (live-verified, see Task 2's real-chart test).
        assert!(object.metadata.labels.is_none());
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&cert_manager_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "cert-manager");
        assert_eq!(reference.namespace, "");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CertManagerReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CertManagerReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CertManagerReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "Namespace".to_string(),
            name: "cert-manager".to_string(),
            timeout: Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        let validation = CertManagerReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CertManagerReconcileError::NotLeader.failure_reason(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cert_manager_reconciler:: 2>&1 | tail -40`
Expected: FAIL to compile — `validate`, `cert_manager_namespace_object`, `ValidationError`, `CertManagerReconcileError` are not defined yet.

- [ ] **Step 3: Write the implementation**

Insert above the test module in `src/cert_manager_reconciler.rs`:

```rust
#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "CertManagerInstallation {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] CertManagerSpecError),
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

pub fn validate(name: &str, spec: &CertManagerInstallationSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::cert_manager::validate_cert_manager(spec)?;
    Ok(())
}

/// The cert-manager chart renders no `Namespace` object, so the controller
/// synthesizes one and applies it ahead of everything else. It is also
/// tracked in `status.appliedResources` so prune semantics stay consistent.
///
/// Unlike Calico's `tigera-operator` namespace (hostPath CNI plugin binaries)
/// and Spegel's `spegel` namespace (containerd socket, host paths), this
/// namespace carries no `pod-security.kubernetes.io/*` labels at all:
/// cert-manager's controller/webhook/cainjector Deployments run with
/// `runAsNonRoot: true`, a seccomp profile, `allowPrivilegeEscalation: false`
/// and every capability dropped (live-verified 2026-09-28 rendering chart
/// v1.16.2) -- restricted-PSS-safe, so Talos's default `baseline` policy
/// admits them unmodified.
pub fn cert_manager_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::CERT_MANAGER_NAMESPACE,
        },
    }))
    .expect("static Namespace JSON deserializes into a DynamicObject")
}

#[derive(thiserror::Error, Debug)]
pub enum CertManagerReconcileError {
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

impl CertManagerReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CertManagerReconcileError::Helm(_) => Some("RenderFailed"),
            CertManagerReconcileError::Manifest(_) => Some("InvalidManifest"),
            CertManagerReconcileError::Apply(_) => Some("ApplyFailed"),
            CertManagerReconcileError::Validation(_)
            | CertManagerReconcileError::Status(_)
            | CertManagerReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status`
/// (with a reason and the ledger of everything that may exist) before the
/// original error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, CertManagerReconcileError> {
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
    obj: &CertManagerInstallation,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CertManagerInstallation> = kube::Api::all(ctx.client.clone());
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
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CertManagerReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CertManagerInstallation> = kube::Api::all(ctx.client.clone());
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
        return Err(CertManagerReconcileError::Validation(err));
    }
    tracing::info!(
        installation = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::cert_manager::build_values(&obj.spec);
    let rendered =
        crate::helm::render_chart(&crate::helm::CERT_MANAGER_CHART, &chart_version, &values).await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::CERT_MANAGER_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered cert-manager chart"
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
    // add nothing, so they write nothing.
    let mut desired = vec![crate::apply::resource_ref(&cert_manager_namespace_object())];
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
        &cert_manager_namespace_object(),
        "platform-controller",
    )
    .await?;
    applied.push(namespace_ref);

    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
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
    _obj: Arc<CertManagerInstallation>,
    _err: &kube::runtime::finalizer::Error<CertManagerReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<CertManagerInstallation>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CertManagerReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CertManagerInstallationStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CertManagerReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, CertManagerReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CertManagerReconcileError::NotLeader);
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

    // Reverse ledger order: the namespace was applied first, so it goes last.
    // Nothing this component applies needs a bounded removal wait: it creates
    // no cert-manager.io custom resource itself.
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
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CertManagerReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all;
    // see `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CertManagerInstallation> = kube::Api::all(ctx.client.clone());
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

Run: `cargo test --lib cert_manager_reconciler:: 2>&1 | tail -40`
Expected: PASS, 8 tests.

- [ ] **Step 5: Run the whole library test suite**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS, no regressions.

- [ ] **Step 6: Commit**

```bash
git add src/cert_manager_reconciler.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add the CertManagerInstallation reconciler

Same shape as cache_reconciler.rs: validate, render, synthesize and
apply the cert-manager namespace, apply the rendered chart, prune,
and a plain reverse-ledger-order cleanup (no removal waits needed).
The synthesized namespace carries no pod-security labels, unlike
Calico/Spegel -- the chart's pods are restricted-PSS-safe.

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
- Consumes: `crate::cert_manager::CertManagerInstallation` (Task 1), `crate::cert_manager_reconciler::{reconcile_with_finalizer, error_policy}` (Task 3).
- Produces: nothing new — this task only wires existing pieces together.

- [ ] **Step 1: Add the CRD to `generated_yaml`**

In `src/crds.rs`, add `crate::cert_manager::CertManagerInstallation::crd(),` as the fifth entry in the array inside `generated_yaml`:

```rust
pub fn generated_yaml() -> String {
    [
        crate::crd::CniInstallation::crd(),
        crate::pull_through_cache::PullThroughCache::crd(),
        crate::cloud_controller_manager::CloudControllerManager::crd(),
        crate::csi_driver::CsiDriver::crd(),
        crate::cert_manager::CertManagerInstallation::crd(),
    ]
    .iter()
    .map(|crd| serde_yaml::to_string(crd).expect("CRD should serialize to YAML"))
    .collect::<Vec<_>>()
    .join("---\n")
}
```

- [ ] **Step 2: Update the CRD-count test's expected names**

In `tests/bootstrap_manifests.rs`, change `crd_yaml_defines_all_platform_resources`'s expected `names` to add the fifth entry, in the same order as `generated_yaml`:

```rust
    assert_eq!(
        names,
        vec![
            "cniinstallations.platform.rye.ninja",
            "pullthroughcaches.platform.rye.ninja",
            "cloudcontrollermanagers.platform.rye.ninja",
            "csidrivers.platform.rye.ninja",
            "certmanagerinstallations.platform.rye.ninja",
        ]
    );
```

- [ ] **Step 3: Regenerate `deploy/crd.yaml`**

Run: `cargo run -q --bin crdgen > deploy/crd.yaml`

- [ ] **Step 4: Run the bootstrap-manifest tests to verify the regenerated file matches**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -20`
Expected: PASS — `crd_yaml_defines_all_platform_resources` and `crd_yaml_matches_the_generated_crds` both pass.

- [ ] **Step 5: Add the fifth watcher and Controller in `main.rs`**

Add the import near the top of `src/main.rs`, alongside the other component imports:

```rust
use platform_controller::cert_manager::CertManagerInstallation;
use platform_controller::cert_manager_reconciler;
```

Add, after the `csi_controller` block and before `let mut sigterm = signal(...)`:

```rust
    // The cert-manager component gets its own watcher, store and Controller
    // too, with the same predicate filter and the same Context (one leader lease).
    let cert_manager_api: Api<CertManagerInstallation> = Api::all(client.clone());
    let (cert_manager_reader, cert_manager_writer) = reflector::store();
    let cert_manager_installations = watcher(cert_manager_api, watcher::Config::default())
        .default_backoff()
        .reflect(cert_manager_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let cert_manager_controller = Controller::for_stream(cert_manager_installations, cert_manager_reader)
        .run(
            cert_manager_reconciler::reconcile_with_finalizer,
            cert_manager_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled cert-manager installation"),
                Err(err) => tracing::error!(error = %err, "cert-manager installation reconcile failed"),
            }
        });
```

Change the `context` clone on the `csi_controller`'s own `.run(...)` call from `context` (moved) to `context.clone()`, since it is now used again below:

```rust
    let csi_controller = Controller::for_stream(drivers, csi_reader)
        .run(
            csi_reconciler::reconcile_with_finalizer,
            csi_reconciler::error_policy,
            context.clone(),
        )
```

Add a fifth arm to the `tokio::select!` block:

```rust
        _ = cert_manager_controller => {}
```

- [ ] **Step 6: Add a deletion-requested unit test for the new kind, mirroring the existing four**

In `src/main.rs`'s `#[cfg(test)] mod tests`, add after `deletion_requested_works_for_the_csi_driver_kind_too`:

```rust
    #[test]
    fn deletion_requested_works_for_the_cert_manager_installation_kind_too() {
        let installation: CertManagerInstallation = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CertManagerInstallation",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-28T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "chartVersion": "v1.16.2"
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
Expected: PASS across the library, `main.rs`'s own tests, and every integration file (the two ignored real-cluster tests are skipped by default).

- [ ] **Step 8: Commit**

```bash
git add src/crds.rs src/main.rs tests/bootstrap_manifests.rs deploy/crd.yaml
git commit -m "$(cat <<'EOF'
feat: register CertManagerInstallation's CRD and controller

Fifth CRD in crdgen's output; fifth watcher/Controller in main.rs,
sharing the existing leader lease and predicate filter. Regenerates
deploy/crd.yaml.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Example manifest

**Files:**
- Create: `examples/cert-manager.yaml`
- Create: `tests/cert_manager_example.rs`

**Interfaces:**
- Consumes: `platform_controller::cert_manager::{build_values, CertManagerInstallation}`, `platform_controller::cert_manager_reconciler::validate`, `platform_controller::manifests::parse_manifests` (all from earlier tasks).
- Produces: nothing new — this task only adds a starting-point manifest and its test.

- [ ] **Step 1: Write the example manifest**

Create `examples/cert-manager.yaml`:

```yaml
# Sample CertManagerInstallation for a self-hosted Talos Linux cluster:
# installs cert-manager, the standard Kubernetes operator for issuing and
# renewing X.509 certificates. This resource only installs cert-manager
# itself -- it does NOT configure any ClusterIssuer/Issuer (self-signed CA,
# ACME, DNS-01 solvers). Configure those directly against the running
# cert-manager once this resource reaches Ready; see
# docs/runbooks/cert-manager-verification.md for a self-signed smoke test.
#
# The chart runs cert-manager on the pod network, so apply this after the CNI
# is Ready (a CniInstallation that has reached Ready).
#
#   kubectl apply -f examples/cert-manager.yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CertManagerInstallation
metadata:
  name: default
spec:
  platformKind: talos-linux
  # Unlike the OpenStack charts, cert-manager's chart and app versions track
  # together and both carry the "v" prefix. If .status shows Failed /
  # RenderFailed, an app-only or un-prefixed version is the usual cause.
  chartVersion: "v1.16.2"
```

- [ ] **Step 2: Write the example test**

Create `tests/cert_manager_example.rs`:

```rust
use platform_controller::cert_manager::{build_values, CertManagerInstallation};
use platform_controller::cert_manager_reconciler;
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/cert-manager.yaml";

fn load() -> CertManagerInstallation {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a CertManagerInstallation")
}

#[test]
fn example_is_a_single_cert_manager_installation_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CertManagerInstallation");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    cert_manager_reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_chart_version_carries_the_v_prefix() {
    // Unlike the OpenStack charts (no "v" prefix on the chart version), the
    // cert-manager chart and app versions track together and both use "v".
    let version = load().spec.chart_version;

    assert!(version.starts_with('v'), "{version}");
}

#[test]
fn example_values_always_enable_crds() {
    let values = build_values(&load().spec);

    assert_eq!(values["crds"]["enabled"], true);
}
```

- [ ] **Step 3: Run the example tests**

Run: `cargo test --test cert_manager_example 2>&1 | tail -20`
Expected: PASS, 4 tests.

- [ ] **Step 4: Commit**

```bash
git add examples/cert-manager.yaml tests/cert_manager_example.rs
git commit -m "$(cat <<'EOF'
feat: add the CertManagerInstallation example manifest

A Talos starting point (chartVersion v1.16.2), plus a test that it
parses, validates and builds the expected values.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Integration test, docs and memory

**Files:**
- Create: `tests/integration_cert_manager.rs`
- Modify: `deploy/README.md`
- Create: `docs/runbooks/cert-manager-verification.md`
- Create: `docs/memory/cert-manager-2026-09.md`
- Modify: `docs/memory/MEMORY.md`

**Interfaces:**
- Consumes: `platform_controller::cert_manager::CertManagerInstallation`, `platform_controller::crd::Phase` (Tasks 1, 4).
- Produces: nothing new — this task only adds the ignored integration test, deployment docs, the runbook and the memory entry.

- [ ] **Step 1: Write the ignored integration test**

Create `tests/integration_cert_manager.rs`:

```rust
// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running and all five CRDs Established, and CniInstallation
// already Ready:
//
//   kubectl apply -f examples/cert-manager.yaml
//   cargo test --test integration_cert_manager -- --ignored --nocapture
//
// The test deletes the CertManagerInstallation at the end, so re-apply the
// example to run it again. It does NOT configure any ClusterIssuer/Certificate
// smoke test; that is a manual runbook step
// (docs/runbooks/cert-manager-verification.md).

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::cert_manager::CertManagerInstallation;
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
async fn cert_manager_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let installations: Api<CertManagerInstallation> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "cert-manager");

    eventually("CertManagerInstallation reaching Ready", Duration::from_secs(300), || async {
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
        .expect("should list Deployments in the cert-manager namespace");
    assert_eq!(
        listed.items.len(),
        3,
        "Ready but the expected 3 Deployments (controller, webhook, cainjector) are not all present: {:?}",
        listed.items.iter().filter_map(|d| d.metadata.name.clone()).collect::<Vec<_>>()
    );

    installations
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the CertManagerInstallation");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        installations.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the cert-manager namespace disappearing", Duration::from_secs(120), || async {
        namespaces.get_opt("cert-manager").await.expect("get_opt should succeed").is_none()
    })
    .await;
}
```

- [ ] **Step 2: Verify it compiles (it will not run without a live cluster)**

Run: `cargo test --test integration_cert_manager --no-run 2>&1 | tail -20`
Expected: compiles with no errors; no tests are executed (`--no-run`).

- [ ] **Step 3: Update `deploy/README.md`**

Add a sixth `kubectl wait` line after the `csidrivers` line in the top apply-order block:

```
kubectl wait --for=condition=established --timeout=60s crd/certmanagerinstallations.platform.rye.ninja
```

Update every `**Upgrading an existing install:**` note in the file (there are three: under Pull-through image cache, Cloud controller manager, and CSI driver) to say "all five CRDs" instead of "both CRDs" / "all four CRDs".

Add a new section after "## CSI driver (OpenStack Cinder, optional)" and before "## Calico node address autodetection":

```markdown
## Cert-manager (optional)

`examples/cert-manager.yaml` is a `CertManagerInstallation` that installs
[cert-manager](https://cert-manager.io) on a self-hosted Talos cluster.
Unlike the other optional components, it needs no cloud-specific
prerequisite and no user-created Secret: apply it whenever cert-manager
itself is wanted.

This resource only installs cert-manager -- it does **not** configure any
`ClusterIssuer`/`Issuer` (self-signed CA, ACME, per-cloud DNS-01 solvers).
Configure those directly against the running cert-manager once
`CertManagerInstallation` reports `Ready`; see
`docs/runbooks/cert-manager-verification.md` for a self-signed smoke test
that proves the installed chart is actually functional.

`spec.chartVersion` is the Helm chart version (`v1.16.2`). Unlike the
OpenStack charts, cert-manager's chart and app versions track together and
both carry the `v` prefix -- do not drop it here.

**Apply order:** after `CniInstallation` is `Ready` (cert-manager's pods run
on the pod network and need cluster DNS, same reasoning as
`PullThroughCache`). No dependency on `CloudControllerManager` or
`CsiDriver`.

**Deleting** a `CertManagerInstallation` removes the chart's objects
(including its CRDs) but does not touch any `Certificate`/`Issuer`/
`ClusterIssuer` a person created against it -- those become orphaned data
with no controller renewing them.
```

- [ ] **Step 4: Verify `deploy/README.md`'s code fences still match reality**

Run: `grep -c "certmanagerinstallations.platform.rye.ninja" deploy/README.md`
Expected: `1`.

- [ ] **Step 5: Write the runbook**

Create `docs/runbooks/cert-manager-verification.md`:

```markdown
# Verifying cert-manager on Talos

Manual acceptance for the `CertManagerInstallation` resource. Needs a real
cluster (the Talos-in-Docker setup in `tests/integration_talos.rs` is
enough) with `CniInstallation` already `Ready`. Nothing here has been run
yet: record what you observe under "Findings to record" at the end, the way
`pull-through-cache-verification.md` does.

## 1. Apply and reach Ready

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/certmanagerinstallations.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cert-manager.yaml
kubectl get certmgr default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n cert-manager get pods -o wide
```

Expected: `Ready`; three pods (`cert-manager`, `cert-manager-webhook`,
`cert-manager-cainjector`), all Running.

## 2. Namespace admission under the default (`baseline`) Pod Security Standard

The design assumes cert-manager's pods need no `pod-security.kubernetes.io/*`
labels on their namespace (confirmed by rendering the chart, not by running
it against Talos). Confirm the pods actually started with no admission
rejection and no privilege elevation:

```sh
kubectl get namespace cert-manager -o jsonpath='{.metadata.labels}{"\n"}'   # no pod-security labels
kubectl -n cert-manager get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
kubectl -n cert-manager get pod -l app=cert-manager -o jsonpath='{.items[0].spec.securityContext}{"\n"}'
```

Expected: no `pod-security.kubernetes.io/*` label on the namespace; every pod
`Running`; `runAsNonRoot: true` present. If any pod is stuck `Pending` with an
admission error mentioning Pod Security, this assumption was wrong --
`cert_manager_namespace_object` in `src/cert_manager_reconciler.rs` needs the
same `privileged` labels Calico's and Spegel's namespaces carry, and this
runbook and the design spec both need updating to say so.

## 3. Self-signed ClusterIssuer and Certificate smoke test

This resource does not configure any Issuer; this step is purely to prove
the installed chart actually issues certificates, not part of what
`CertManagerInstallation` manages.

```sh
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: selfsigned-smoketest
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: smoketest
  namespace: default
spec:
  secretName: smoketest-tls
  issuerRef:
    name: selfsigned-smoketest
    kind: ClusterIssuer
  commonName: smoketest.local
  dnsNames:
    - smoketest.local
EOF
kubectl get certificate smoketest -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl get secret smoketest-tls
EOF
kubectl delete certificate smoketest
kubectl delete clusterissuer selfsigned-smoketest
```

Expected: `Ready=True` within a few seconds, and `smoketest-tls` exists
holding a TLS Secret. If this fails, the webhook or CA injection isn't
actually working even though `CertManagerInstallation` reports `Ready`
("manifests applied" only, never a health check).

## 4. Delete, and what stays

```sh
kubectl delete certmgr default        # returns once the finalizer clears
kubectl get namespace cert-manager    # NotFound
```

Expected: the chart's objects (including its CRDs) are gone. Any
`Certificate`/`Issuer`/`ClusterIssuer` a person created independently (like
the smoke test above, if not cleaned up) becomes orphaned: nothing renews it
anymore.

## When something goes wrong

- `kubectl get certmgr default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidHelmValues`,
  `Unsupported`; `RenderFailed` when helm cannot render the chart, e.g. an
  app-only or un-prefixed chart version; `InvalidManifest`; `ApplyFailed`
  when the API server rejects an object) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is
  applied, so deleting the resource after a failed first install still
  removes everything that was created.

## Findings to record

To fill in from the first live run: whether the namespace-admission
assumption in step 2 held, the smoke-test result in step 3, and anything in
step 4 that differs from "Expected".
```

- [ ] **Step 6: Write the memory entry**

Create `docs/memory/cert-manager-2026-09.md`:

```markdown
---
name: cert-manager-2026-09
description: CertManagerInstallation slice, 2026-09-28 - fifth CRD, no provider enum, chart facts confirmed by live rendering (not yet run against a cluster)
metadata:
  type: project
---

`CertManagerInstallation` (cluster-scoped singleton `default`, shortname
`certmgr`) installs cert-manager on Talos. Spec:
`docs/superpowers/specs/2026-09-28-cert-manager-installation-design.md`;
plan: `docs/superpowers/plans/2026-09-28-cert-manager-installation.md`; live
acceptance: `docs/runbooks/cert-manager-verification.md`. Unlike every other
component here, its spec has **no `provider` field** -- there is no second
implementation of "the thing that installs cert-manager" to model, so the
spec is flat (`platformKind`, `chartVersion`, `helmValues`). It also
installs cert-manager only: no `ClusterIssuer`/`Issuer` configuration, which
is deliberately out of scope (see the spec's Non-goals) and left for a
future, separate component.

**Non-obvious facts, from rendering the real chart (`v1.16.2`), not
assumed:**
- `crds.enabled` **defaults to `false`** on this chart -- rendering without
  it produces zero CRDs. `build_values` forces it `true` unconditionally, a
  real requirement rather than defensive redundancy. With it true, 6 CRDs
  render: `certificaterequests`/`certificates`/`challenges.acme`/
  `clusterissuers`/`issuers`/`orders.acme`, all `.cert-manager.io`.
- The chart renders **no `Namespace`** and **no `Secret`** object, same as
  Calico's tigera-operator chart and Spegel.
- Exactly 3 Deployments (`cert-manager-cainjector`, `cert-manager`,
  `cert-manager-webhook`), 1 `ValidatingWebhookConfiguration`
  (`cert-manager-webhook`), 1 `MutatingWebhookConfiguration`.
- Every container sets `runAsNonRoot: true`, a `seccompProfile`,
  `allowPrivilegeEscalation: false` and drops every capability --
  **restricted**-PSS-safe, not merely baseline-safe. So
  `cert_manager_namespace_object` sets **no**
  `pod-security.kubernetes.io/*` labels, unlike `tigera-operator` (Calico)
  and `spegel` (both `privileged`, for hostPath/host-network reasons that
  don't apply here). This is a stated assumption pending the runbook's step
  2 -- not yet live-verified against Talos's actual admission behavior.
- Like Spegel, the OCI chart prints `Pulled:`/`Digest:` preamble lines to
  stdout; `helm::strip_oci_pull_preamble` already handles this generically,
  no code change needed.
- Unlike the OpenStack charts (chart version has no `v` prefix, app version
  does), cert-manager's chart and app versions **track together and both
  carry `v`** (`v1.16.2`). Documented in the example and its test so this
  doesn't get "corrected" the wrong way by someone used to the CCM/CSI
  convention.
- No `cleanupTimeoutSeconds`, no `wait_for_object_kind` call needed: this
  component creates no `cert-manager.io` custom resource itself, so there's
  no same-reconcile ordering dependency the way CNI has on the tigera
  operator's own CRDs.

**Verification status:** all Rust code (Tasks 1-6) built and the full test
suite passed in a scratch worktree, including the two `#[ignore]`d
real-chart tests in `helm.rs` against the live `quay.io` chart. **Nothing
has been run against an actual cluster yet** -- the namespace-admission
assumption above, the delete/cleanup behavior, and the self-signed
`ClusterIssuer`/`Certificate` smoke test are all still open per the
runbook.

**How to apply:** when bumping the chart version, re-render it
(`helm template cert-manager oci://quay.io/jetstack/charts/cert-manager
--version <new> --include-crds --no-hooks --namespace cert-manager --set
crds.enabled=true`) and re-check the CRD list, the Deployment/webhook
counts, and whether a `Namespace`/`Secret`/hook object starts appearing;
re-run the two ignored real-chart tests in `helm.rs`.
```

- [ ] **Step 7: Update the memory index**

Append one line to `docs/memory/MEMORY.md` (after the `CsiDriver` line):

```markdown
- [CertManagerInstallation slice](cert-manager-2026-09.md) — fifth CRD, no provider enum, installs cert-manager only (no Issuer config); crds.enabled defaults false on this chart; namespace needs no privileged labels (restricted-PSS-safe pods); nothing live-verified against a cluster yet
```

- [ ] **Step 8: Run the full test suite and clippy one more time**

Run: `cargo test 2>&1 | tail -60`
Expected: PASS.

Run: `cargo clippy --all-targets 2>&1 | tail -60`
Expected: only the same warning classes the existing code already triggers (`result_large_err`, `too_many_arguments`, and any pre-existing ones in `apply.rs`/`manifests.rs`) — no new categories.

- [ ] **Step 9: Commit**

```bash
git add tests/integration_cert_manager.rs deploy/README.md docs/runbooks/cert-manager-verification.md docs/memory/cert-manager-2026-09.md docs/memory/MEMORY.md
git commit -m "$(cat <<'EOF'
docs: add cert-manager deployment docs, runbook and memory entry

Ignored integration test (apply/Ready/delete/cleanup); deploy/README.md
gains a Cert-manager section and the fifth CRD's establish-wait; a
runbook covering the namespace-admission assumption and a self-signed
ClusterIssuer/Certificate smoke test; and a memory entry recording the
live-rendered chart facts this plan relied on.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
