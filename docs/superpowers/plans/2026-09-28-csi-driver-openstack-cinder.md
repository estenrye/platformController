# CSI Driver (OpenStack Cinder) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a fourth platform component to the controller: a `CsiDriver` custom resource that installs the `openstack-cinder-csi` Helm chart (block storage) on self-hosted Talos clusters running on OpenStack VMs.

**Architecture:** A new cluster-scoped CRD, `CsiDriver` — but, unlike the other three components, not a `name: default` singleton. Because a single cloud can need several CSI drivers running at once (e.g. AWS wants both `ebs.csi.aws.com` and `efs.csi.aws.com`), the reconciler keeps the existing "one chart per CR" shape and instead allows multiple CR instances, one per driver, each named after its driver (`openstack-cinder` for the one driver built here). Its own reconciler (`src/csi_reconciler.rs`) renders the `openstack-cinder-csi` Helm chart from the same classic chart repository `CloudControllerManager` already uses, server-side-applies it into `kube-system`, and prunes and cleans up through the same primitives the other three reconcilers use. The OpenStack credentials stay in a user-created Secret that the CR only names, the same pattern `CloudControllerManager` already established. `main.rs` runs a fourth `Controller` under the same leader lease.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, the `helm` CLI (already a runtime dependency), chart `openstack-cinder-csi` `2.36.5` (app `v1.36.0`) from `https://kubernetes.github.io/cloud-provider-openstack` — the same repository and chart version line as `CloudControllerManager`'s `openstack-cloud-controller-manager` chart.

**Spec:** [docs/superpowers/specs/2026-09-28-csi-driver-openstack-cinder-design.md](../specs/2026-09-28-csi-driver-openstack-cinder-design.md)

## Global Constraints

Every task's requirements implicitly include this section. Values are copied from the spec, plus facts confirmed by rendering the real chart (`helm template cinder-csi cpo/openstack-cinder-csi --version 2.36.5 --namespace kube-system -f <values> --no-hooks --include-crds`) while writing this plan.

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `CsiDriver`, **cluster-scoped**, shortname `csi`, plural `csidrivers`. **Not a singleton.** Each CR manages exactly one driver; `metadata.name` must equal that driver's canonical name (`openstack-cinder` for `driver: openstackCinder`), enforced in code (a cross-field constraint the CRD schema can't express).
- `platformKind` accepts only `talos-linux`; `driver` accepts only `openstackCinder`. There is no driver check in code beyond the name match: single-variant enum, serde already rejects anything else.
- `chartVersion` is the Helm **chart** version (`2.36.5`), not the app version (`v1.36.0`). Required, no default, passed verbatim to `helm --version`; leading/trailing whitespace is rejected.
- `cloudConfigSecretRef.name` is required and must be a valid Kubernetes object name (DNS-1123 subdomain). The Secret lives in `kube-system` and must hold the whole cloud config under the key `cloud.conf` (the chart's default `secret.filename`). The controller never reads it.
- `defaultStorageClass` is an enum `delete | retain | none`, default `delete`. The controller always sets **both** `storageClass.delete.isDefault` and `storageClass.retain.isDefault` from this one field (`delete` → `true`/`false`, `retain` → `false`/`true`, `none` → `false`/`false`), so a `helmValues` passthrough can never create two defaults between this chart's own two StorageClasses.
- Values layering: `helmValues` first, then the typed fields (`secret.enabled: true`, `secret.create: false`, `secret.hostMount: false`, `secret.name: <ref>`, both `storageClass.*.isDefault` flags). Later layers always win.
- `helmValues` must not set `secret.data`: `secret.create` is always forced `false`, so the chart's Secret template — the only consumer of `secret.data` — never renders, and the value would sit as leaked credentials in a CR that is not a Secret. Rejected as `InvalidHelmValues`.
- No `dnsPolicy` or `extraVolumes`/`extraVolumeMounts` override, unlike `CloudControllerManager`: rendering shows the node plugin's `hostNetwork`/`dnsPolicy: ClusterFirstWithHostNet` and its `/etc/cacert` hostPath mount don't create the CCM's chicken-and-egg deadlock, because a `CsiDriver` reconcile never gates CNI/CoreDNS scheduling.
- Chart: repo `https://kubernetes.github.io/cloud-provider-openstack` (same repo as the CCM chart), chart `openstack-cinder-csi`, release name `cinder-csi` (it appears in the node DaemonSet's and controller Deployment's immutable `selector`, so never change it), namespace `kube-system`. Render with `--include-crds --no-hooks --namespace kube-system` (the existing `render_chart`/`render_args` path is reused unchanged).
- Rendered objects (verified by rendering `2.36.5` with `secret.enabled=true, secret.create=false, secret.hostMount=false, secret.name=cloud-config`): 2 `ServiceAccount`s (`csi-cinder-controller-sa`, `csi-cinder-node-sa`), 5 `ClusterRole`/`ClusterRoleBinding` pairs (`csi-attacher-role`, `csi-provisioner-role`, `csi-snapshotter-role`, `csi-resizer-role`, `csi-nodeplugin-role`), one node-plugin `DaemonSet` (`openstack-cinder-csi-nodeplugin`; `hostNetwork: true`, no node selector, tolerates every taint — `- operator: Exists` with no key), one controller-plugin `Deployment` (`openstack-cinder-csi-controllerplugin`; 1 replica, **no `hostNetwork`, no tolerations at all** — runs on the regular pod network), one `CSIDriver` object named `cinder.csi.openstack.org`, and two `StorageClass`es (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`). No `Secret` object is rendered. The Secret is mounted at `/etc/config`, read from `CLOUD_CONFIG=/etc/config/cloud.conf`.
- **Ordering consequence of the fact above:** unlike the CCM DaemonSet, the controller-plugin Deployment has no toleration for `node.cloudprovider.kubernetes.io/uninitialized` *and* runs on the pod network, so it needs both `CloudControllerManager` (to clear the taint) and `CniInstallation` (for a working pod network and cluster DNS) before it can schedule and run. No code enforces this; it's a documented apply-order note. No toleration is added anywhere — eventually consistent, not a hard failure.
- No `cleanupTimeoutSeconds` on this CRD.
- The finalizer name `platform.rye.ninja/cleanup` is reused (finalizers are per object).
- The Secret's existence is not checked. `Ready` means "manifests applied", not "driver healthy".
- The four components apply in this documented order: `CloudControllerManager`, `CniInstallation`, `CsiDriver`, `PullThroughCache`. The controller enforces no ordering.
- Existing CNI, cache and CCM tests must pass unchanged, except `crd_yaml_defines_all_platform_resources`, which is extended to four CRDs. `SecretNameRef` and its DNS-1123 validator move from `src/cloud_controller_manager.rs` to `src/crd.rs` (Task 1, Step 1) so both components share one definition; `crate::cloud_controller_manager::SecretNameRef` keeps working via a re-export, so no other file changes.
- CI runs `cargo build`, `cargo test`, `cargo clippy --all-targets`. Clippy warnings are not denied; `result_large_err` already fires on the existing reconcilers and the new one triggers it the same way — accepted.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Building needs free disk (a debug build of this crate is several GB). If a build fails with `No space left on device`, free space first; do not delete anything that is not a build artifact.
- CLAUDE.md: project memory lives in `docs/memory/` (index in `docs/memory/MEMORY.md`), committed like any other change. Do not write to the out-of-repo memory path.

## Review Focus

Failure modes the spec implies but that are easiest to miss. Each has a test in the task that owns the code.

1. A `CsiDriver` named `default` (the habit every other CRD in this codebase trains) instead of `openstack-cinder`. It must get `Failed`/`Unsupported`, not silently be ignored or misapplied. Test in Task 3.
2. A `helmValues` passthrough that tries to leak credentials via `secret.data`, which `secret.create: false` makes inert. Test in Task 1.
3. A `helmValues` passthrough that tries to override `storageClass.delete.isDefault`/`storageClass.retain.isDefault` directly, fighting the typed `defaultStorageClass` field. The typed layer must win for all three enum values (`delete`, `retain`, `none`). Tests in Task 1.
4. A chart bump that changes the rendered shape the spec relies on (a Secret object appears, the mount key or path changes, the controller-plugin gains `hostNetwork`/a toleration, a hook is rendered). The ignored real-chart test in Task 2 pins it; re-run it on every chart bump.
5. A standby replica, or a validation failure, must not overwrite status with a generic failure. Test in Task 3.

---

## Branch

The spec and this plan are committed on `main`. Create the implementation branch from there:

```bash
git checkout -b csi-driver-openstack-cinder
```

---

## File Structure

| File | Responsibility |
|---|---|
| `src/crd.rs` (modify) | `SecretNameRef` and its DNS-1123 name validator move here from `cloud_controller_manager.rs`, shared by both components. |
| `src/cloud_controller_manager.rs` (modify) | Local `SecretNameRef`/validator definitions replaced with a re-export from `crd.rs`; no behavior change. |
| `src/csi_driver.rs` (create) | `CsiDriver` CRD types, status type, `Driver` enum and its expected-name mapping, `DefaultStorageClass` enum, spec validation, the values builder. |
| `src/csi_reconciler.rs` (create) | Reconcile, cleanup, finalizer wiring, status for `CsiDriver`. |
| `src/helm.rs` (modify) | Rename `OPENSTACK_CCM_CHART_REPO` to `CLOUD_PROVIDER_OPENSTACK_CHART_REPO` (now shared by two charts); add `CINDER_CSI_NAMESPACE`, `OPENSTACK_CINDER_CSI_CHART`; render-args test; ignored real-chart test. |
| `src/lib.rs` (modify) | Register the two new modules. |
| `src/crds.rs` (modify) | Include the fourth CRD in `generated_yaml()`. |
| `src/main.rs` (modify) | Fourth watcher and `Controller<CsiDriver>`. |
| `deploy/crd.yaml` (regenerate) | All four CRDs. |
| `deploy/README.md` (modify) | Establish-wait for the fourth CRD, apply order, upgrade notes, CSI driver section. |
| `examples/csi-driver-openstack-cinder.yaml` (create) | Talos-on-OpenStack starting point. |
| `tests/bootstrap_manifests.rs` (modify) | Four CRDs. |
| `tests/csi_driver_example.rs` (create) | The example parses, validates and builds the expected values. |
| `tests/integration_csi_driver.rs` (create) | Ignored: apply, assert the DaemonSet and Deployment, delete, assert cleanup. |
| `docs/runbooks/csi-driver-openstack-cinder-verification.md` (create) | Live acceptance checks. |
| `docs/memory/csi-driver-openstack-cinder-2026-09.md`, `docs/memory/MEMORY.md`, `docs/memory/rbac-cluster-admin-tradeoff.md` | Memory entry, index line, ledger rows. |

---

### Task 1: Shared Secret-name types and the CsiDriver CRD

**Files:**
- Modify: `src/crd.rs`, `src/cloud_controller_manager.rs`, `src/lib.rs`
- Create: `src/csi_driver.rs`
- Test: inline `#[cfg(test)]` modules in `src/crd.rs` (existing) and `src/csi_driver.rs`

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`; `crate::pull_through_cache::{merge, preserve_unknown_object}` (already `pub(crate)`).
- Produces:
  - `crate::crd::SecretNameRef { name: String }` (moved here; derives `Serialize, Deserialize, Clone, Debug, JsonSchema, Default`) and `pub(crate) fn is_dns1123_subdomain(name: &str) -> bool` (moved here).
  - `CsiDriver` (the CRD kind), `CsiDriverSpec { platform_kind: PlatformKind, driver: Driver, openstack_cinder: OpenstackCinderSpec }`, `CsiDriverStatus { phase, observed_generation: i64, chart_version: String, applied_resources: Vec<AppliedResourceRef>, conditions: Vec<Condition> }`
  - `Driver::OpenstackCinder`, with `Driver::expected_name(&self) -> &'static str`
  - `DefaultStorageClass { Delete, Retain, None }`, default `Delete`
  - `OpenstackCinderSpec { chart_version: String, cloud_config_secret_ref: SecretNameRef, default_storage_class: DefaultStorageClass, helm_values: Option<serde_json::Value> }` (derives `Default`)
  - `CsiSpecError` with `reason(&self) -> &'static str` returning `"InvalidChartVersion"`, `"InvalidSecretRef"` or `"InvalidHelmValues"`
  - `validate_openstack_cinder(&OpenstackCinderSpec) -> Result<(), CsiSpecError>`
  - `build_values(&OpenstackCinderSpec) -> serde_json::Value`

- [ ] **Step 1: Move the shared Secret-name types into `crd.rs`**

In `src/cloud_controller_manager.rs`, delete the `SecretNameRef` struct (lines 44-48) and the `is_dns1123_subdomain` function (lines 102-115), and delete their two blank-line gaps. Replace the `use` line at the top with:

```rust
pub use crate::crd::SecretNameRef;
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
```

The `pub use` keeps `crate::cloud_controller_manager::SecretNameRef` resolving for the two other files that name it that way (`src/helm.rs:537` and `src/ccm_reconciler.rs:374`), so neither needs to change. The plain `use` makes `SecretNameRef` resolve unqualified everywhere it's already used in this file (the `cloud_config_secret_ref: SecretNameRef` field and the two `SecretNameRef { name: ... }` constructions), so those keep compiling unchanged too.

Change the one call site that used the deleted local function — in `validate_openstack`, replace:

```rust
    if !is_dns1123_subdomain(&openstack.cloud_config_secret_ref.name) {
```

with:

```rust
    if !crate::crd::is_dns1123_subdomain(&openstack.cloud_config_secret_ref.name) {
```

In `src/crd.rs`, add near the top (after the existing `use` lines, before `CniInstallationSpec`):

```rust
/// A reference to a Secret by name only: the namespace is fixed by whichever
/// chart consumes it.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
pub struct SecretNameRef {
    pub name: String,
}

/// A Kubernetes object name: a DNS-1123 subdomain. One or more dot-separated
/// labels of lowercase alphanumerics and '-', each starting and ending with an
/// alphanumeric, at most 253 characters in all.
pub(crate) fn is_dns1123_subdomain(name: &str) -> bool {
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
```

Run: `cargo test --lib cloud_controller_manager:: 2>&1 | tail -20`
Expected: PASS, unchanged (this step only moves code; no test's assertions change).

- [ ] **Step 2: Write the failing tests**

Create `src/csi_driver.rs` containing only the test module below, and add `pub mod csi_driver;` to `src/lib.rs` after `pub mod cloud_controller_manager;`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::SecretNameRef;
    use kube::CustomResourceExt;

    fn openstack_cinder() -> OpenstackCinderSpec {
        OpenstackCinderSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: SecretNameRef {
                name: "cloud-config".to_string(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn spec_deserializes_with_only_the_required_fields() {
        let spec: CsiDriverSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "driver": "openstackCinder",
            "openstackCinder": {
                "chartVersion": "2.36.5",
                "cloudConfigSecretRef": { "name": "cloud-config" }
            }
        }))
        .expect("minimal spec should deserialize");

        assert_eq!(spec.platform_kind, PlatformKind::TalosLinux);
        assert_eq!(spec.driver, Driver::OpenstackCinder);
        assert_eq!(spec.openstack_cinder.chart_version, "2.36.5");
        assert_eq!(spec.openstack_cinder.cloud_config_secret_ref.name, "cloud-config");
        assert_eq!(spec.openstack_cinder.default_storage_class, DefaultStorageClass::Delete);
        assert!(spec.openstack_cinder.helm_values.is_none());
    }

    #[test]
    fn unknown_drivers_are_rejected_at_deserialization() {
        let result = serde_json::from_value::<CsiDriverSpec>(serde_json::json!({
            "platformKind": "talos-linux",
            "driver": "awsEbs",
            "openstackCinder": {
                "chartVersion": "2.36.5",
                "cloudConfigSecretRef": { "name": "cloud-config" }
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn a_spec_without_a_secret_reference_is_rejected_at_deserialization() {
        let result = serde_json::from_value::<CsiDriverSpec>(serde_json::json!({
            "platformKind": "talos-linux",
            "driver": "openstackCinder",
            "openstackCinder": { "chartVersion": "2.36.5" }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn openstack_cinder_expects_the_name_openstack_dash_cinder() {
        assert_eq!(Driver::OpenstackCinder.expected_name(), "openstack-cinder");
    }

    #[test]
    fn accepts_a_minimal_spec() {
        assert_eq!(validate_openstack_cinder(&openstack_cinder()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_chart_version() {
        let mut spec = openstack_cinder();
        spec.chart_version = "  ".to_string();

        let err = validate_openstack_cinder(&spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err, CsiSpecError::EmptyChartVersion);
        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn rejects_chart_versions_with_leading_or_trailing_whitespace() {
        for version in [" 2.36.5", "2.36.5 ", "2.36.5\n"] {
            let mut spec = openstack_cinder();
            spec.chart_version = version.to_string();

            let err = validate_openstack_cinder(&spec).expect_err("whitespace must be rejected");

            assert_eq!(err, CsiSpecError::ChartVersionHasWhitespace(version.to_string()));
            assert_eq!(err.reason(), "InvalidChartVersion");
        }
    }

    #[test]
    fn accepts_secret_names_kubernetes_accepts() {
        for name in ["cloud-config", "a", "my.secret-1", "0abc", &"a".repeat(253)] {
            let mut spec = openstack_cinder();
            spec.cloud_config_secret_ref.name = name.to_string();

            assert_eq!(validate_openstack_cinder(&spec), Ok(()), "{name}");
        }
    }

    #[test]
    fn rejects_secret_names_kubernetes_rejects() {
        for name in ["", "Cloud-Config", "cloud_config", "-cloud-config", "cloud-config-", &"a".repeat(254)] {
            let mut spec = openstack_cinder();
            spec.cloud_config_secret_ref.name = name.to_string();

            let err = validate_openstack_cinder(&spec).expect_err("invalid Secret name must be rejected");

            assert_eq!(err, CsiSpecError::InvalidSecretName(name.to_string()), "{name:?}");
            assert_eq!(err.reason(), "InvalidSecretRef");
        }
    }

    #[test]
    fn rejects_helm_values_that_are_not_an_object() {
        for value in [serde_json::json!("nope"), serde_json::json!(["a"]), serde_json::json!(3), serde_json::json!(null)] {
            let mut spec = openstack_cinder();
            spec.helm_values = Some(value.clone());

            let err = validate_openstack_cinder(&spec).expect_err("non-object helmValues is invalid");

            assert_eq!(err, CsiSpecError::HelmValuesNotObject, "{value}");
            assert_eq!(err.reason(), "InvalidHelmValues");
        }
    }

    #[test]
    fn rejects_secret_data_smuggled_through_helm_values() {
        // secret.create is always forced to false, so the chart's Secret template
        // never renders: secret.data has no effect on the running driver. A user
        // who set it would have their OpenStack credentials sit, with no effect,
        // in a cluster-scoped CR that is not a Secret. Reject it outright.
        let mut spec = openstack_cinder();
        spec.helm_values = Some(serde_json::json!({
            "secret": { "data": { "cloud.conf": "[Global]\nauth-url=..." } }
        }));

        let err = validate_openstack_cinder(&spec).expect_err("secret.data via helmValues is invalid");

        assert_eq!(err, CsiSpecError::SecretDataInHelmValues);
        assert_eq!(err.reason(), "InvalidHelmValues");
    }

    #[test]
    fn accepts_helm_values_that_do_not_touch_secret_data() {
        let mut spec = openstack_cinder();
        spec.helm_values = Some(serde_json::json!({ "clusterID": "prod", "secret": { "annotations": { "a": "b" } } }));

        assert_eq!(validate_openstack_cinder(&spec), Ok(()));
    }

    #[test]
    fn values_point_the_chart_at_the_existing_secret_never_create_or_host_mount_one() {
        let values = build_values(&openstack_cinder());

        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["hostMount"], false);
        assert_eq!(values["secret"]["name"], "cloud-config");
    }

    #[test]
    fn default_storage_class_delete_marks_the_delete_class_default() {
        let values = build_values(&openstack_cinder());

        assert_eq!(values["storageClass"]["delete"]["isDefault"], true);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], false);
    }

    #[test]
    fn default_storage_class_retain_marks_the_retain_class_default() {
        let mut spec = openstack_cinder();
        spec.default_storage_class = DefaultStorageClass::Retain;

        let values = build_values(&spec);

        assert_eq!(values["storageClass"]["delete"]["isDefault"], false);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], true);
    }

    #[test]
    fn default_storage_class_none_marks_neither_class_default() {
        let mut spec = openstack_cinder();
        spec.default_storage_class = DefaultStorageClass::None;

        let values = build_values(&spec);

        assert_eq!(values["storageClass"]["delete"]["isDefault"], false);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], false);
    }

    #[test]
    fn typed_values_win_over_conflicting_helm_values() {
        let mut spec = openstack_cinder();
        spec.helm_values = Some(serde_json::json!({
            "secret": { "create": true, "enabled": false, "hostMount": true, "name": "other" },
            "storageClass": { "delete": { "isDefault": false }, "retain": { "isDefault": true } }
        }));

        let values = build_values(&spec);

        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["hostMount"], false);
        assert_eq!(values["secret"]["name"], "cloud-config");
        assert_eq!(values["storageClass"]["delete"]["isDefault"], true);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], false);
    }

    #[test]
    fn helm_values_pass_through_alongside_the_typed_values() {
        let mut spec = openstack_cinder();
        spec.helm_values = Some(serde_json::json!({
            "clusterID": "prod",
            "logVerbosityLevel": 4,
            "secret": { "annotations": { "a": "b" } }
        }));

        let values = build_values(&spec);

        assert_eq!(values["clusterID"], "prod");
        assert_eq!(values["logVerbosityLevel"], 4);
        // Merging is recursive: untouched siblings under `secret` survive.
        assert_eq!(values["secret"]["annotations"]["a"], "b");
        assert_eq!(values["secret"]["name"], "cloud-config");
    }

    #[test]
    fn crd_is_cluster_scoped_with_the_csi_shortname() {
        let crd = CsiDriver::crd();

        assert_eq!(crd.metadata.name.as_deref(), Some("csidrivers.platform.rye.ninja"));
        assert_eq!(crd.spec.scope, "Cluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["csi".to_string()]));
    }

    #[test]
    fn crd_schema_lets_helm_values_carry_arbitrary_keys() {
        let yaml = serde_yaml::to_string(&CsiDriver::crd()).expect("CRD serializes");

        assert!(
            yaml.contains("x-kubernetes-preserve-unknown-fields: true"),
            "helmValues must be a structural, unknown-field-preserving object:\n{yaml}"
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib csi_driver:: 2>&1 | tail -20`
Expected: FAIL to compile — nothing above `mod tests` is defined yet.

- [ ] **Step 4: Write the implementation**

Prepend the following above the `#[cfg(test)]` block in `src/csi_driver.rs` (the full file is the code below followed by the unchanged test module from Step 2):

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind, SecretNameRef};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CsiDriver",
    status = "CsiDriverStatus",
    shortname = "csi"
)]
#[serde(rename_all = "camelCase")]
pub struct CsiDriverSpec {
    pub platform_kind: PlatformKind,
    pub driver: Driver,
    pub openstack_cinder: OpenstackCinderSpec,
}

/// Unlike `CniInstallation`'s or `CloudControllerManager`'s singleton, a
/// `CsiDriver` is not a `name: default` singleton: a cloud can need several
/// drivers installed at once (e.g. AWS's EBS and EFS drivers together), so
/// each driver gets its own CR instance instead of nesting many drivers under
/// one. `expected_name` is the cross-field constraint that keeps at most one
/// CR managing a given driver: the CRD schema can't express "name must equal
/// this other field", so `csi_reconciler::validate` checks it at runtime.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Driver {
    OpenstackCinder,
}

impl Driver {
    pub fn expected_name(&self) -> &'static str {
        match self {
            Driver::OpenstackCinder => "openstack-cinder",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenstackCinderSpec {
    /// The Helm chart version of `openstack-cinder-csi` (for example
    /// `2.36.5`), not the application version (`v1.36.0`).
    pub chart_version: String,
    /// The Secret holding the OpenStack cloud config. It must exist in
    /// `kube-system` and hold the whole config under the key `cloud.conf`. The
    /// controller passes only its name to the chart and never reads it.
    pub cloud_config_secret_ref: SecretNameRef,
    /// Which of the chart's two StorageClasses (`csi-cinder-sc-delete`,
    /// `csi-cinder-sc-retain`) is the cluster's default. The controller always
    /// sets both `storageClass.*.isDefault` flags from this one field, so a
    /// `helmValues` passthrough can never create two defaults between them.
    #[serde(default)]
    pub default_storage_class: DefaultStorageClass,
    /// Free-form values merged into the chart's values. Typed fields are
    /// overlaid on top, so they always win.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::pull_through_cache::preserve_unknown_object")]
    pub helm_values: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DefaultStorageClass {
    #[default]
    Delete,
    Retain,
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CsiDriverStatus {
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
pub enum CsiSpecError {
    #[error("spec.openstackCinder.chartVersion must not be empty")]
    EmptyChartVersion,
    #[error("spec.openstackCinder.chartVersion {0:?} has leading or trailing whitespace")]
    ChartVersionHasWhitespace(String),
    #[error(
        "spec.openstackCinder.cloudConfigSecretRef.name {0:?} is not a valid Kubernetes object \
         name (lowercase alphanumerics, '-' and '.', starting and ending with an alphanumeric, \
         at most 253 characters)"
    )]
    InvalidSecretName(String),
    #[error("spec.openstackCinder.helmValues must be a JSON object")]
    HelmValuesNotObject,
    #[error(
        "spec.openstackCinder.helmValues must not set secret.data: secret.create is always \
         false, so the chart never renders a Secret from it and it has no effect; use \
         spec.openstackCinder.cloudConfigSecretRef instead"
    )]
    SecretDataInHelmValues,
}

impl CsiSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            CsiSpecError::EmptyChartVersion | CsiSpecError::ChartVersionHasWhitespace(_) => {
                "InvalidChartVersion"
            }
            CsiSpecError::InvalidSecretName(_) => "InvalidSecretRef",
            CsiSpecError::HelmValuesNotObject | CsiSpecError::SecretDataInHelmValues => {
                "InvalidHelmValues"
            }
        }
    }
}

/// Rejects specs the chart would silently mis-handle, before anything is
/// applied.
pub fn validate_openstack_cinder(spec: &OpenstackCinderSpec) -> Result<(), CsiSpecError> {
    if spec.chart_version.trim().is_empty() {
        return Err(CsiSpecError::EmptyChartVersion);
    }
    if spec.chart_version.trim() != spec.chart_version {
        return Err(CsiSpecError::ChartVersionHasWhitespace(spec.chart_version.clone()));
    }
    if !crate::crd::is_dns1123_subdomain(&spec.cloud_config_secret_ref.name) {
        return Err(CsiSpecError::InvalidSecretName(spec.cloud_config_secret_ref.name.clone()));
    }
    if let Some(values) = &spec.helm_values {
        if !values.is_object() {
            return Err(CsiSpecError::HelmValuesNotObject);
        }
        if values.get("secret").and_then(|secret| secret.get("data")).is_some() {
            return Err(CsiSpecError::SecretDataInHelmValues);
        }
    }
    Ok(())
}

/// The Helm values for the openstack-cinder-csi chart: the user's
/// `helmValues` passthrough, with the typed fields overlaid on top.
///
/// - The Secret is user-created, so the chart must use it, never create one,
///   and never fall back to the host-path `/etc/cloud/cloud.conf` the chart
///   would otherwise read by default (`secret.hostMount`).
/// - `defaultStorageClass` deterministically sets both StorageClasses'
///   `isDefault` flags, so at most one of this chart's own two StorageClasses
///   is ever the cluster default.
pub fn build_values(spec: &OpenstackCinderSpec) -> serde_json::Value {
    let mut values = spec.helm_values.clone().unwrap_or_else(|| serde_json::json!({}));

    let (delete_is_default, retain_is_default) = match spec.default_storage_class {
        DefaultStorageClass::Delete => (true, false),
        DefaultStorageClass::Retain => (false, true),
        DefaultStorageClass::None => (false, false),
    };

    let typed = serde_json::json!({
        "secret": {
            "enabled": true,
            "create": false,
            "hostMount": false,
            "name": spec.cloud_config_secret_ref.name,
        },
        "storageClass": {
            "delete": { "isDefault": delete_is_default },
            "retain": { "isDefault": retain_is_default },
        },
    });

    crate::pull_through_cache::merge(&mut values, typed);
    values
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib csi_driver:: 2>&1 | tail -30`
Expected: PASS (20 tests).
Run: `cargo build 2>&1 | tail -10`
Expected: builds (confirms the `cloud_controller_manager.rs` re-export in Step 1 satisfies `helm.rs` and `ccm_reconciler.rs`).

- [ ] **Step 6: Commit**

```bash
git add src/crd.rs src/cloud_controller_manager.rs src/csi_driver.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add the CsiDriver CRD, validation and values builder

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Chart reference and real-chart render test

**Files:**
- Modify: `src/helm.rs`

**Interfaces:**
- Consumes: `csi_driver::{OpenstackCinderSpec, build_values}` (Task 1); `crd::SecretNameRef` (Task 1).
- Produces: `helm::CLOUD_PROVIDER_OPENSTACK_CHART_REPO`, `helm::CINDER_CSI_NAMESPACE`, `helm::OPENSTACK_CINDER_CSI_CHART: ChartRef`, both consumed by `csi_reconciler` in Task 3.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/helm.rs`, after `openstack_ccm_render_args_use_the_repo_form_and_kube_system`:

```rust
    #[test]
    fn openstack_cinder_csi_render_args_use_the_repo_form_and_kube_system() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&OPENSTACK_CINDER_CSI_CHART, "2.36.5", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "cinder-csi".to_string(),
                "--repo".to_string(),
                "https://kubernetes.github.io/cloud-provider-openstack".to_string(),
                "openstack-cinder-csi".to_string(),
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

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib helm:: 2>&1 | tail -10`
Expected: FAIL to compile — `OPENSTACK_CINDER_CSI_CHART` is not defined.

- [ ] **Step 3: Add the chart reference**

In `src/helm.rs`, replace:

```rust
/// The Helm repository the OpenStack cloud-controller-manager chart is fetched from.
pub const OPENSTACK_CCM_CHART_REPO: &str = "https://kubernetes.github.io/cloud-provider-openstack";
```

with:

```rust
/// The Helm repository both OpenStack charts (cloud-controller-manager and
/// cinder-csi) are fetched from.
pub const CLOUD_PROVIDER_OPENSTACK_CHART_REPO: &str = "https://kubernetes.github.io/cloud-provider-openstack";

/// Namespace the openstack-cinder-csi chart is released into. Same reasoning
/// as `CCM_NAMESPACE`: the chart mounts the cloud-config Secret from its own
/// release namespace, and `kube-system` already exists on Talos, so no
/// namespace is synthesized.
pub const CINDER_CSI_NAMESPACE: &str = "kube-system";
```

In the `OPENSTACK_CCM_CHART` definition, replace:

```rust
        url: OPENSTACK_CCM_CHART_REPO,
```

with:

```rust
        url: CLOUD_PROVIDER_OPENSTACK_CHART_REPO,
```

Add, directly after the `OPENSTACK_CCM_CHART` const definition:

```rust
/// The release name is part of the node DaemonSet's and controller
/// Deployment's immutable `selector` (the chart labels pods `release:
/// <name>`), so it must never change for an installed cluster.
pub const OPENSTACK_CINDER_CSI_CHART: ChartRef = ChartRef {
    release: "cinder-csi",
    source: ChartSource::Repo {
        url: CLOUD_PROVIDER_OPENSTACK_CHART_REPO,
        chart: "openstack-cinder-csi",
    },
    namespace: CINDER_CSI_NAMESPACE,
};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib helm:: 2>&1 | tail -15`
Expected: PASS. `cargo build 2>&1 | tail -5` still builds (the rename's only other use site was updated in this step).

- [ ] **Step 5: Add the ignored real-chart test**

Add to the `tests` module in `src/helm.rs`, after `openstack_ccm_chart_renders_the_shape_the_spec_relies_on`:

```rust
    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn openstack_cinder_csi_chart_renders_the_shape_the_spec_relies_on() {
        let openstack_cinder = crate::csi_driver::OpenstackCinderSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: crate::crd::SecretNameRef {
                name: "my-cloud-config".to_string(),
            },
            ..Default::default()
        };
        let values = crate::csi_driver::build_values(&openstack_cinder);

        let rendered = render_chart(&OPENSTACK_CINDER_CSI_CHART, &openstack_cinder.chart_version, &values)
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
        assert_eq!(kinds.iter().filter(|kind| **kind == "Deployment").count(), 1, "{kinds:?}");
        assert!(kinds.contains(&"CSIDriver"), "{kinds:?}");

        let csi_driver_object = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "CSIDriver")
            .expect("one CSIDriver");
        assert_eq!(csi_driver_object.metadata.name.as_deref(), Some("cinder.csi.openstack.org"));

        let node_plugin = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "DaemonSet")
            .expect("one DaemonSet");
        assert_eq!(node_plugin.metadata.namespace.as_deref(), Some("kube-system"));

        let volumes = node_plugin
            .data
            .pointer("/spec/template/spec/volumes")
            .and_then(|value| value.as_array())
            .expect("the DaemonSet has volumes");
        let secret_volume = volumes
            .iter()
            .find(|volume| volume.get("secret").is_some())
            .expect("a Secret volume");
        assert_eq!(secret_volume["secret"]["secretName"], "my-cloud-config");

        // The chart reads the config from the `cloud.conf` key of that Secret.
        assert!(rendered.contains("/etc/config/cloud.conf"), "{rendered}");

        // Unlike the node plugin, the controller plugin has no hostNetwork and no
        // toleration: it needs a working CNI and the uninitialized taint cleared
        // before it can schedule and run. This is why the deploy README documents
        // CsiDriver as applying after both CloudControllerManager and CniInstallation.
        let controller_plugin = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "Deployment")
            .expect("one Deployment");
        assert!(
            controller_plugin.data.pointer("/spec/template/spec/hostNetwork").is_none(),
            "the controller plugin runs on the pod network"
        );
        let controller_tolerations = controller_plugin
            .data
            .pointer("/spec/template/spec/tolerations")
            .and_then(|value| value.as_array());
        assert!(
            controller_tolerations.is_none_or(|tolerations| tolerations.is_empty()),
            "{controller_tolerations:?}"
        );

        // defaultStorageClass defaults to `delete`: only csi-cinder-sc-delete is
        // annotated as the cluster default.
        let delete_class = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-delete"))
            .expect("the delete-reclaim StorageClass");
        let retain_class = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-retain"))
            .expect("the retain-reclaim StorageClass");
        assert_eq!(
            delete_class
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("storageclass.kubernetes.io/is-default-class"))
                .map(String::as_str),
            Some("true")
        );
        assert!(
            retain_class
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("storageclass.kubernetes.io/is-default-class"))
                .is_none()
        );

        // --no-hooks: no hook objects are rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }
```

- [ ] **Step 6: Run it against the real chart**

Run: `cargo test --lib -- --ignored openstack_cinder_csi 2>&1 | tail -15`
Expected: PASS (needs network access and the `helm` CLI).

- [ ] **Step 7: Commit**

```bash
git add src/helm.rs
git commit -m "$(cat <<'EOF'
feat: add the openstack-cinder-csi chart reference and render test

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: The reconciler

**Files:**
- Create: `src/csi_reconciler.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` module in `src/csi_reconciler.rs`

**Interfaces:**
- Consumes: Task 1's types, `validate_openstack_cinder`, `build_values`; Task 2's `helm::OPENSTACK_CINDER_CSI_CHART` and `helm::render_chart`; existing `crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME}`, `crate::apply::{apply_object, delete_object, resource_ref, resources_to_prune, wait_for_crd_established}`, `crate::ledger::{ReconcileProgress, checkpoint_ledger, failure_ledger}`, `crate::manifests::{parse_manifests, sort_manifests, is_custom_resource}`.
- Produces:
  - `csi_reconciler::validate(name: &str, spec: &CsiDriverSpec) -> Result<(), ValidationError>`
  - `csi_reconciler::ValidationError` with `reason(&self) -> &'static str`
  - `csi_reconciler::CsiReconcileError` with `failure_reason(&self) -> Option<&'static str>`
  - `csi_reconciler::reconcile_with_finalizer(Arc<CsiDriver>, Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<CsiReconcileError>>` and `csi_reconciler::error_policy(Arc<CsiDriver>, &finalizer::Error<CsiReconcileError>, Arc<Context>) -> Action`, both passed to `Controller::run` in Task 4.

- [ ] **Step 1: Write the failing tests**

Create `src/csi_reconciler.rs` containing only this test module, and add `pub mod csi_reconciler;` to `src/lib.rs` after `pub mod csi_driver;`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::SecretNameRef;
    use crate::csi_driver::{Driver, OpenstackCinderSpec};

    fn spec_with(platform_kind: PlatformKind) -> CsiDriverSpec {
        CsiDriverSpec {
            platform_kind,
            driver: Driver::OpenstackCinder,
            openstack_cinder: OpenstackCinderSpec {
                chart_version: "2.36.5".to_string(),
                cloud_config_secret_ref: SecretNameRef {
                    name: "cloud-config".to_string(),
                },
                ..Default::default()
            },
        }
    }

    #[test]
    fn accepts_talos_linux_openstack_cinder_named_openstack_cinder() {
        assert!(validate("openstack-cinder", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_csi_drivers_not_named_for_their_driver() {
        for name in ["default", "cinder", "Openstack-Cinder", ""] {
            let err = validate(name, &spec_with(PlatformKind::TalosLinux))
                .expect_err("a name other than the driver's expected name should be rejected");

            assert!(matches!(
                &err,
                ValidationError::UnsupportedName { given, expected, .. }
                    if given == name && *expected == "openstack-cinder"
            ));
            assert_eq!(err.reason(), "Unsupported");
        }
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack_cinder.chart_version = String::new();
        let err = validate("openstack-cinder", &spec).expect_err("blank chartVersion is invalid");
        assert_eq!(err.reason(), "InvalidChartVersion");

        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack_cinder.cloud_config_secret_ref.name = "Cloud_Config".to_string();
        let err = validate("openstack-cinder", &spec).expect_err("bad Secret name is invalid");
        assert_eq!(err.reason(), "InvalidSecretRef");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CsiReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CsiReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CsiReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "DaemonSet".to_string(),
            name: "openstack-cinder-csi-nodeplugin".to_string(),
            timeout: std::time::Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        // Validation already writes its own Failed status; a standby must never
        // write status at all.
        let validation = CsiReconcileError::Validation(ValidationError::UnsupportedName {
            given: "default".to_string(),
            driver: Driver::OpenstackCinder,
            expected: "openstack-cinder",
        });

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CsiReconcileError::NotLeader.failure_reason(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib csi_reconciler:: 2>&1 | tail -20`
Expected: FAIL to compile — nothing above `mod tests` is defined yet.

- [ ] **Step 3: Write the implementation**

Prepend the following above the `#[cfg(test)]` block in `src/csi_reconciler.rs`:

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::csi_driver::{CsiDriver, CsiDriverSpec, CsiDriverStatus, CsiSpecError, Driver};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "CsiDriver {given:?} is ignored; a CsiDriver with driver {driver:?} must be named {expected:?}"
    )]
    UnsupportedName {
        given: String,
        driver: Driver,
        expected: &'static str,
    },
    #[error(transparent)]
    Spec(#[from] CsiSpecError),
}

impl ValidationError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            ValidationError::UnsupportedPlatform(_) | ValidationError::UnsupportedName { .. } => {
                "Unsupported"
            }
        }
    }
}

/// There is no driver check beyond the name match: `Driver` has one variant
/// and serde already rejects any other value. Add one alongside the second
/// driver. Unlike the other three CRDs, `CsiDriver` is not a `name: default`
/// singleton: the name must equal the driver's own expected name instead, so
/// at most one CR can ever manage a given driver.
pub fn validate(name: &str, spec: &CsiDriverSpec) -> Result<(), ValidationError> {
    let expected = spec.driver.expected_name();
    if name != expected {
        return Err(ValidationError::UnsupportedName {
            given: name.to_string(),
            driver: spec.driver.clone(),
            expected,
        });
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::csi_driver::validate_openstack_cinder(&spec.openstack_cinder)?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum CsiReconcileError {
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

impl CsiReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CsiReconcileError::Helm(_) => Some("RenderFailed"),
            CsiReconcileError::Manifest(_) => Some("InvalidManifest"),
            CsiReconcileError::Apply(_) => Some("ApplyFailed"),
            CsiReconcileError::Validation(_)
            | CsiReconcileError::Status(_)
            | CsiReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(obj: Arc<CsiDriver>, ctx: Arc<Context>) -> Result<Action, CsiReconcileError> {
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
    obj: &CsiDriver,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CsiDriver> = kube::Api::all(ctx.client.clone());
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
        &obj.spec.openstack_cinder.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(csi_driver = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<CsiDriver>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CsiReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CsiDriver> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.openstack_cinder.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(csi_driver = %name, error = %err, "validation failed");
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
        return Err(CsiReconcileError::Validation(err));
    }
    tracing::info!(
        csi_driver = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::csi_driver::build_values(&obj.spec.openstack_cinder);
    let rendered =
        crate::helm::render_chart(&crate::helm::OPENSTACK_CINDER_CSI_CHART, &chart_version, &values)
            .await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::CINDER_CSI_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered openstack-cinder-csi chart"
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
        // The chart renders neither CRDs nor custom resources today; this guards a
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
    tracing::info!(csi_driver = %name, phase = ?Phase::Ready, "updating status");
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
    _obj: Arc<CsiDriver>,
    _err: &kube::runtime::finalizer::Error<CsiReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    api: &kube::Api<CsiDriver>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CsiReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CsiDriverStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CsiReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(obj: Arc<CsiDriver>, ctx: Arc<Context>) -> Result<Action, CsiReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CsiReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(csi_driver = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order. There are no removal waits. Deleting this CR does
    // not delete already-provisioned Cinder volumes; PVCs or pods still
    // depending on them can be left with a stuck detach/unmount.
    for reference in applied.iter().rev() {
        tracing::info!(
            csi_driver = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(csi_driver = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<CsiDriver>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CsiReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all; see
    // `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CsiDriver> = kube::Api::all(ctx.client.clone());
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

Run: `cargo test --lib csi_reconciler:: 2>&1 | tail -20`
Expected: PASS (7 tests).
Run: `cargo build 2>&1 | tail -5`
Expected: builds.

- [ ] **Step 5: Commit**

```bash
git add src/csi_reconciler.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add the CsiDriver reconciler

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Register the CRD and run the fourth controller

**Files:**
- Modify: `src/crds.rs`, `src/main.rs`, `tests/bootstrap_manifests.rs`
- Regenerate: `deploy/crd.yaml`
- Test: `tests/bootstrap_manifests.rs`, inline tests in `src/main.rs`

**Interfaces:**
- Consumes: `CsiDriver` (Task 1); `csi_reconciler::{reconcile_with_finalizer, error_policy}` (Task 3).
- Produces: `crds::generated_yaml()` now emits four CRDs in the order CniInstallation, PullThroughCache, CloudControllerManager, CsiDriver; a running fourth `Controller`.

- [ ] **Step 1: Update the CRD tests so they fail**

In `tests/bootstrap_manifests.rs`, replace the `names` assertion in `crd_yaml_defines_all_platform_resources` with:

```rust
    assert_eq!(
        names,
        vec![
            "cniinstallations.platform.rye.ninja",
            "pullthroughcaches.platform.rye.ninja",
            "cloudcontrollermanagers.platform.rye.ninja",
            "csidrivers.platform.rye.ninja",
        ]
    );
```

In `src/main.rs`'s `tests` module, add:

```rust
    #[test]
    fn deletion_requested_works_for_the_csi_driver_kind_too() {
        let driver: CsiDriver = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CsiDriver",
            "metadata": {
                "name": "openstack-cinder",
                "deletionTimestamp": "2026-09-28T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "driver": "openstackCinder",
                "openstackCinder": {
                    "chartVersion": "2.36.5",
                    "cloudConfigSecretRef": { "name": "cloud-config" }
                }
            }
        }))
        .expect("driver should deserialize");

        assert_eq!(deletion_requested(&driver), Some(1));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -20`
Expected: FAIL: `crd_yaml_defines_all_platform_resources` (three names, not four). `cargo test --bin platform-controller 2>&1 | tail` FAILS to compile: `CsiDriver` not in scope.

- [ ] **Step 3: Register the CRD and regenerate the file**

In `src/crds.rs`, add the fourth entry:

```rust
    [
        crate::crd::CniInstallation::crd(),
        crate::pull_through_cache::PullThroughCache::crd(),
        crate::cloud_controller_manager::CloudControllerManager::crd(),
        crate::csi_driver::CsiDriver::crd(),
    ]
```

Regenerate: `cargo run -q --bin crdgen > deploy/crd.yaml`

- [ ] **Step 4: Run the fourth controller**

In `src/main.rs`, add to the imports:

```rust
use platform_controller::csi_driver::CsiDriver;
use platform_controller::csi_reconciler;
```

Change the CCM controller to clone the shared context (it currently moves it):

```rust
            ccm_reconciler::error_policy,
            context.clone(),
```

Add, after the `ccm_controller` definition and before `let mut sigterm`:

```rust
    // The CSI driver component gets its own watcher, store and Controller too,
    // with the same predicate filter and the same Context (one leader lease).
    let csi_api: Api<CsiDriver> = Api::all(client.clone());
    let (csi_reader, csi_writer) = reflector::store();
    let drivers = watcher(csi_api, watcher::Config::default())
        .default_backoff()
        .reflect(csi_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let csi_controller = Controller::for_stream(drivers, csi_reader)
        .run(
            csi_reconciler::reconcile_with_finalizer,
            csi_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled csi driver"),
                Err(err) => tracing::error!(error = %err, "csi driver reconcile failed"),
            }
        });
```

Add a branch to the `select!`, after `_ = ccm_controller => {}`:

```rust
        _ = csi_controller => {}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -10`
Expected: PASS (5 tests, including the four-CRD assertion).
Run: `cargo test --bin platform-controller 2>&1 | tail -10`
Expected: PASS (5 tests).
Run: `cargo build 2>&1 | tail -5`
Expected: builds.

- [ ] **Step 6: Commit**

```bash
git add src/crds.rs src/main.rs tests/bootstrap_manifests.rs deploy/crd.yaml
git commit -m "$(cat <<'EOF'
feat: register the CsiDriver CRD and run its controller

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Deploy artifacts and the example

**Files:**
- Modify: `deploy/README.md`
- Create: `examples/csi-driver-openstack-cinder.yaml`, `tests/csi_driver_example.rs`
- Test: `tests/csi_driver_example.rs`

**Interfaces:**
- Consumes: `csi_reconciler::validate`, `csi_driver::{build_values, CsiDriver}` (Tasks 1, 3).
- Produces: nothing later tasks call; the example is referenced by the runbook (Task 6) as `examples/csi-driver-openstack-cinder.yaml`.

- [ ] **Step 1: Write the failing tests**

Create `tests/csi_driver_example.rs`:

```rust
use platform_controller::csi_driver::{build_values, CsiDriver};
use platform_controller::csi_reconciler;
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/csi-driver-openstack-cinder.yaml";

fn load() -> CsiDriver {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a CsiDriver")
}

#[test]
fn example_is_a_single_csi_driver_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CsiDriver");
}

#[test]
fn example_is_named_for_its_driver_and_passes_validation() {
    let driver = load();

    assert_eq!(driver.metadata.name.as_deref(), Some("openstack-cinder"));
    csi_reconciler::validate("openstack-cinder", &driver.spec).expect("the example must be valid");
}

#[test]
fn example_chart_version_is_the_chart_version_not_the_app_version() {
    // The chart is versioned 2.x; the application it deploys is v1.x. Using the
    // app version (or a `v` prefix) fails with "chart ... not found".
    let version = load().spec.openstack_cinder.chart_version;

    assert!(!version.starts_with('v'), "{version}");
    assert!(version.starts_with("2."), "{version}");
}

#[test]
fn example_values_name_the_secret_the_comment_tells_you_to_create() {
    let openstack_cinder = load().spec.openstack_cinder;
    let values = build_values(&openstack_cinder);

    assert_eq!(openstack_cinder.cloud_config_secret_ref.name, "cloud-config");
    assert_eq!(values["secret"]["name"], "cloud-config");
    assert_eq!(values["secret"]["create"], false);
    assert_eq!(values["secret"]["hostMount"], false);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test csi_driver_example 2>&1 | tail -20`
Expected: FAIL: `the example should exist` (the file does not exist yet).

- [ ] **Step 3: Add the example**

Create `examples/csi-driver-openstack-cinder.yaml`:

```yaml
# Sample CsiDriver for a self-hosted Talos Linux cluster running on OpenStack
# VMs: installs the openstack-cinder-csi driver, giving the cluster two
# StorageClasses (csi-cinder-sc-delete, csi-cinder-sc-retain) backed by Cinder
# block volumes. It does nothing on a managed cluster (EKS, GKE, AKS, OKE),
# where the provider already runs its own CSI drivers.
#
# BEFORE applying this:
#
# 1. Apply CloudControllerManager and CniInstallation first (see deploy/README.md,
#    "Apply order"): the controller-plugin Deployment runs on the regular pod
#    network and has no toleration for the uninitialized cloud-provider taint,
#    so it needs both a working CNI and CloudControllerManager's node
#    initialization before it can schedule and run.
#
# 2. Create the cloud config Secret in kube-system. The controller never reads it;
#    it only tells the chart its name. The key MUST be `cloud.conf`:
#
#      kubectl -n kube-system create secret generic cloud-config \
#        --from-file=cloud.conf=./cloud.conf
#
#    cloud.conf is the OpenStack cloud config (auth-url, an application credential,
#    region); see https://github.com/kubernetes/cloud-provider-openstack
#
#   kubectl apply -f examples/csi-driver-openstack-cinder.yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CsiDriver
metadata:
  # Must equal the driver's expected name: "openstack-cinder" for
  # driver: openstackCinder. Unlike the other example CRs this is not
  # "default" -- a CsiDriver is one CR per driver, not a singleton.
  name: openstack-cinder
spec:
  platformKind: talos-linux
  driver: openstackCinder
  openstackCinder:
    # The Helm CHART version of openstack-cinder-csi (2.x), not the
    # application version (v1.x), and with no "v" prefix. If .status shows
    # Failed / RenderFailed, this is the usual cause.
    chartVersion: "2.36.5"
    cloudConfigSecretRef:
      name: cloud-config
    # csi-cinder-sc-delete is the cluster's default StorageClass. Set to
    # "retain" or "none" to choose differently.
    defaultStorageClass: delete
```

- [ ] **Step 4: Update the deploy README**

In `deploy/README.md`, change the apply block to wait on the fourth CRD:

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/cloudcontrollermanagers.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/csidrivers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cni-installation.yaml
```

Change "`crd.yaml` (all three CRDs)" to "`crd.yaml` (all four CRDs)" (both occurrences: the paragraph under the apply block, and the Cloud controller manager section's "Upgrading an existing install" note). Then add this section after the "Cloud controller manager (optional)" section:

```markdown
## CSI driver (OpenStack Cinder, optional)

`examples/csi-driver-openstack-cinder.yaml` is a `CsiDriver` that installs the
`openstack-cinder-csi` driver (block storage) on a self-hosted Talos cluster
running on OpenStack VMs, giving the cluster two StorageClasses
(`csi-cinder-sc-delete`, `csi-cinder-sc-retain`). Skip it on a managed cluster
(EKS, GKE, AKS, OKE): the provider already runs its own CSI drivers.

Unlike `CloudControllerManager`, `CsiDriver` is **not a `name: default`
singleton**: a cloud can need several drivers installed at once (this
controller only builds OpenStack Cinder today), so each CR manages exactly one
driver and its name must equal that driver's own expected name --
`openstack-cinder` for `driver: openstackCinder`. A CR with any other name is
rejected (`Unsupported`).

It needs a Secret named as `spec.openstackCinder.cloudConfigSecretRef.name` in
`kube-system`, holding the OpenStack cloud config under the key `cloud.conf`.
The controller never reads it, and does not check that it exists: with the
Secret missing the driver's pods sit in `ContainerCreating` while
`.status.phase` still says `Ready` (which means "manifests applied").

`spec.openstackCinder.chartVersion` is the Helm chart version (`2.36.5`), not
the application version (`v1.36.0`). `spec.openstackCinder.defaultStorageClass`
(`delete`, `retain` or `none`; default `delete`) picks which StorageClass, if
any, is the cluster default.

**Apply order:** `CloudControllerManager`, then `CniInstallation`, then
`CsiDriver`, then `PullThroughCache`. The controller enforces no ordering, but
unlike the CCM's DaemonSet, the CSI driver's controller-plugin Deployment runs
on the regular pod network and has no toleration for the `uninitialized` taint
-- it needs both the CNI and the CCM's node initialization to actually run,
even though the reconcile that applies its manifests will succeed regardless.

**Upgrading an existing install:** apply the new `deploy/crd.yaml` (and wait
for all four CRDs to be Established) *before* rolling the controller image.

**Deleting** a `CsiDriver` removes the chart's objects but does not delete
already-provisioned Cinder volumes; PVCs or pods still depending on them can be
left with a stuck detach/unmount.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test csi_driver_example 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add deploy/README.md examples/csi-driver-openstack-cinder.yaml tests/csi_driver_example.rs
git commit -m "$(cat <<'EOF'
docs: add the CsiDriver example and deploy README section

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Integration test, runbook and memory

**Files:**
- Create: `tests/integration_csi_driver.rs`, `docs/runbooks/csi-driver-openstack-cinder-verification.md`, `docs/memory/csi-driver-openstack-cinder-2026-09.md`
- Modify: `docs/memory/MEMORY.md`, `docs/memory/rbac-cluster-admin-tradeoff.md`
- Test: the ignored integration test compiles (`cargo test --no-run`)

**Interfaces:**
- Consumes: `CsiDriver` (Task 1); `examples/csi-driver-openstack-cinder.yaml` (Task 5).
- Produces: nothing later tasks use.

- [ ] **Step 1: Write the ignored integration test**

Create `tests/integration_csi_driver.rs`:

```rust
// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running and all four CRDs Established:
//
//   kubectl apply -f examples/csi-driver-openstack-cinder.yaml
//   cargo test --test integration_csi_driver -- --ignored --nocapture
//
// The test deletes the CsiDriver at the end, so re-apply the example to run it
// again. It does NOT assert that the driver pods run: without the cloud-config
// Secret and an OpenStack to talk to, they cannot start. That is a manual
// runbook step (docs/runbooks/csi-driver-openstack-cinder-verification.md).

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use kube::api::{Api, DeleteParams};
use kube::Client;
use platform_controller::crd::Phase;
use platform_controller::csi_driver::CsiDriver;
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
async fn openstack_cinder_csi_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let drivers: Api<CsiDriver> = Api::all(client.clone());
    let daemon_sets: Api<DaemonSet> = Api::namespaced(client.clone(), "kube-system");
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    eventually("CsiDriver reaching Ready", Duration::from_secs(300), || async {
        drivers
            .get("openstack-cinder")
            .await
            .ok()
            .and_then(|driver| driver.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    assert!(
        daemon_sets
            .get_opt("openstack-cinder-csi-nodeplugin")
            .await
            .expect("get_opt should succeed")
            .is_some(),
        "Ready but the node plugin DaemonSet does not exist in kube-system"
    );
    assert!(
        deployments
            .get_opt("openstack-cinder-csi-controllerplugin")
            .await
            .expect("get_opt should succeed")
            .is_some(),
        "Ready but the controller plugin Deployment does not exist in kube-system"
    );

    drivers
        .delete("openstack-cinder", &DeleteParams::default())
        .await
        .expect("should delete the CsiDriver");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        drivers.get_opt("openstack-cinder").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the node plugin DaemonSet disappearing", Duration::from_secs(120), || async {
        daemon_sets
            .get_opt("openstack-cinder-csi-nodeplugin")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
    eventually("the controller plugin Deployment disappearing", Duration::from_secs(120), || async {
        deployments
            .get_opt("openstack-cinder-csi-controllerplugin")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo test --test integration_csi_driver --no-run 2>&1 | tail -5`
Expected: compiles. Run `cargo test --test integration_csi_driver 2>&1 | tail -5`: `1 ignored`.

- [ ] **Step 3: Write the runbook**

Create `docs/runbooks/csi-driver-openstack-cinder-verification.md`:

````markdown
# Verifying the OpenStack Cinder CSI driver on Talos

Manual acceptance for the `CsiDriver` resource (`driver: openstackCinder`).
Needs a real OpenStack cloud you can boot Talos VMs in, credentials for it, a
`CloudControllerManager` already `Ready` (see
`docs/runbooks/cloud-controller-manager-verification.md`), and a `CniInstallation`
already `Ready`. Nothing here has been run yet: record what you observe under
"Findings to record" at the end.

## 1. Apply order

```sh
kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl get cni default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl get nodes -o custom-columns=NAME:.metadata.name,TAINTS:.spec.taints[*].key
```

Expected: no node still carries `node.cloudprovider.kubernetes.io/uninitialized`.
The controller-plugin Deployment this CR creates has no toleration for that
taint and runs on the regular pod network, so both prerequisites above must
already be `Ready` before it can schedule.

## 2. Create the credentials Secret, then apply

Reuse the same `cloud-config` Secret the CCM runbook created, or create a
separate one if this cluster uses different credentials for storage:

```sh
kubectl apply -f examples/csi-driver-openstack-cinder.yaml
kubectl get csi openstack-cinder -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n kube-system get ds openstack-cinder-csi-nodeplugin -o wide
kubectl -n kube-system get deploy openstack-cinder-csi-controllerplugin -o wide
kubectl -n kube-system get pods -l app=openstack-cinder-csi -o wide
```

Expected: `Ready`; the node plugin DaemonSet has one pod per node, Running; the
controller plugin Deployment has one pod, Running. `Ready` means the manifests
were applied, not that the driver is healthy: the pods are the real signal.
Check `kubectl -n kube-system logs <pod>` for errors reaching Keystone or Cinder.

## 3. StorageClasses and the default

```sh
kubectl get storageclass
```

Expected: `csi-cinder-sc-delete` and `csi-cinder-sc-retain`, both provisioner
`cinder.csi.openstack.org`. With the example's default `defaultStorageClass:
delete`, `csi-cinder-sc-delete` is annotated
`storageclass.kubernetes.io/is-default-class: "true"` and is the one a PVC that
names no `storageClassName` gets.

## 4. Provision, attach, mount, expand

```sh
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: csi-cinder-test
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 1Gi
EOF
kubectl get pvc csi-cinder-test -w        # Bound
kubectl run csi-cinder-test-pod --image=registry.k8s.io/pause:3.10 \
  --overrides='{"spec":{"containers":[{"name":"pause","image":"registry.k8s.io/pause:3.10","volumeMounts":[{"name":"data","mountPath":"/data"}]}],"volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"csi-cinder-test"}}]}}'
kubectl get pod csi-cinder-test-pod -w    # Running
```

Expected: the PVC binds, a Cinder volume appears (`openstack volume list`), and
the pod mounts it. Then expand it:

```sh
kubectl patch pvc csi-cinder-test -p '{"spec":{"resources":{"requests":{"storage":"2Gi"}}}}'
kubectl get pvc csi-cinder-test -o jsonpath='{.status.capacity.storage}{"\n"}'
```

Expected: capacity grows to `2Gi` (`allowVolumeExpansion: true` on both
StorageClasses). Delete the pod and PVC afterwards and confirm the Cinder
volume is removed (`openstack volume list`) for `csi-cinder-sc-delete`, and
confirm it is *not* removed for a PVC against `csi-cinder-sc-retain`.

## 5. Node plugin mounts on Talos

```sh
kubectl -n kube-system get pod -l component=nodeplugin -o name | head -1 | \
  xargs -I{} kubectl -n kube-system exec {} -c cinder-csi-plugin -- mount | grep -c cacert
```

Expected: confirm the `/etc/cacert` hostPath mount and `/var/lib/kubelet`
`kubeletDir` mount behave correctly on Talos (no crash-loop from a missing host
path). Record what you find; nothing is overridden for these today.

## 6. Missing Secret

```sh
kubectl -n kube-system delete secret cloud-config
kubectl -n kube-system delete pod -l app=openstack-cinder-csi
kubectl -n kube-system get pods -l app=openstack-cinder-csi      # ContainerCreating
kubectl get csi openstack-cinder -o jsonpath='{.status.phase}{"\n"}'   # still Ready
```

Expected (and by design): the pods cannot mount the Secret and stay
`ContainerCreating`, while status still says `Ready`. Recreate the Secret and
the pods start.

## 7. Delete, and what stays

```sh
kubectl delete csi openstack-cinder        # returns once the finalizer clears
kubectl -n kube-system get ds openstack-cinder-csi-nodeplugin          # NotFound
kubectl -n kube-system get deploy openstack-cinder-csi-controllerplugin # NotFound
openstack volume list
```

Expected: the chart's objects are gone. Any Cinder volume from a
`csi-cinder-sc-retain` PVC still exists; deleting the `CsiDriver` does not
delete provisioned volumes, and a pod still mounting one at delete time can be
left with a stuck detach/unmount.

## When something goes wrong

- `kubectl get csi openstack-cinder -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidSecretRef`,
  `InvalidHelmValues`, `Unsupported` -- including a `metadata.name` that does not
  match the driver's expected name; `RenderFailed` when helm cannot render the
  chart, e.g. the app version `v1.36.0` used as the chart version;
  `InvalidManifest`; `ApplyFailed` when the API server rejects an object) and
  the error text.
- The ledger (`status.appliedResources`) is saved before anything is applied,
  so deleting the resource after a failed first install still removes
  everything that was created.

## Findings to record

To fill in from the first live run: where the pods scheduled and how long
after both prerequisites were `Ready` (step 1-2), the provision/attach/mount/
expand round trip (step 4), the `/etc/cacert` and `kubeletDir` mount behavior
on Talos (step 5), and anything in step 7 that differs from "Expected".
````

- [ ] **Step 4: Write the memory entry and index line**

Create `docs/memory/csi-driver-openstack-cinder-2026-09.md`:

```markdown
---
name: csi-driver-openstack-cinder-2026-09
description: CsiDriver (OpenStack Cinder) slice, 2026-09-28 - fourth CRD beside CniInstallation, PullThroughCache and CloudControllerManager; not a singleton; chart facts from a real render; nothing live-verified yet
metadata:
  type: project
---

`CsiDriver` (cluster-scoped, shortname `csi`) installs CSI drivers, first the `openstack-cinder-csi` chart (block storage only) for Talos clusters on OpenStack VMs. Spec: `docs/superpowers/specs/2026-09-28-csi-driver-openstack-cinder-design.md`; plan: `docs/superpowers/plans/2026-09-28-csi-driver-openstack-cinder.md`; live acceptance: `docs/runbooks/csi-driver-openstack-cinder-verification.md`. It is a fourth parallel component (own reconciler, fourth `Controller` in `main.rs`, same leader lease), and the fourth copy of the finalizer/status glue -- the case for extracting a shared component framework, already flagged after the third copy ([[cloud-controller-manager-2026-09]]), is stronger now but still deliberately not done here.

**Unlike the other three CRDs, `CsiDriver` is not a `name: default` singleton.** A single cloud can need several CSI drivers running at once (AWS wants both `ebs.csi.aws.com` and `efs.csi.aws.com`), so each CR manages exactly one driver and `metadata.name` must equal that driver's own expected name (`openstack-cinder` for `driver: openstackCinder`) -- a cross-field constraint the CRD schema can't express, checked at runtime in `csi_reconciler::validate`. Manila (OpenStack file storage), AWS, Azure, GCP and OCI drivers are deliberately not built; the spec's "Future providers" table names the enum values and CR names each would use.

**Non-obvious facts (from rendering the real chart, not assumed):**
- Chart `openstack-cinder-csi` `2.36.5` (app `v1.36.0`) lives in the *same* classic repo as the CCM chart, `https://kubernetes.github.io/cloud-provider-openstack` (also home to a third chart, `openstack-manila-csi`, not used here). `chartVersion` is the CHART version, not the app version.
- The chart has four ways to source credentials (`secret.enabled`/`create`/`hostMount`); matching CCM's `cloudConfigSecretRef` pattern needs `enabled: true, create: false, hostMount: false, name: <ref>` -- `hostMount: true` is the chart's *own default*, meaning an unconfigured install silently expects `/etc/cloud/cloud.conf` on the host. With the controller's values, no Secret object renders and the config is read from `/etc/config/cloud.conf` (`secret.filename` defaults to `cloud.conf`).
- Rendered objects: 2 ServiceAccounts, 5 ClusterRole/ClusterRoleBinding pairs, one node-plugin DaemonSet (`openstack-cinder-csi-nodeplugin`; `hostNetwork: true`, tolerates every taint, runs on every node), one controller-plugin Deployment (`openstack-cinder-csi-controllerplugin`; 1 replica, **no hostNetwork, no tolerations at all**), a `CSIDriver` object (`cinder.csi.openstack.org`), and two StorageClasses (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`).
- **The controller-plugin Deployment's lack of hostNetwork/tolerations is the key ordering fact**, unlike CCM: it needs a working CNI (pod network + cluster DNS) *and* the CCM's node initialization (taint cleared) before it can actually run, even though applying the CR and its manifests always succeeds. No toleration was added anywhere for this; it's a documented apply-order note (`CloudControllerManager`, `CniInstallation`, `CsiDriver`, `PullThroughCache`).
- No `dnsPolicy`/`extraVolumes` override was added, unlike CCM: the node plugin's `hostNetwork`+`ClusterFirstWithHostNet` doesn't create CCM's chicken-and-egg deadlock, since this component's own reconcile never gates CNI/CoreDNS scheduling the way CCM's Deployment did.
- `storageClass.delete.isDefault`/`storageClass.retain.isDefault` control the `storageclass.kubernetes.io/is-default-class: "true"` annotation; the controller's `defaultStorageClass` enum (`delete`/`retain`/`none`) always sets *both* flags, so a `helmValues` passthrough can never create two defaults between this chart's own two classes.
- The release name `cinder-csi` is in both the DaemonSet's and the Deployment's immutable `selector` (`release: cinder-csi`); never change it. The shared chart repo constant was renamed from `OPENSTACK_CCM_CHART_REPO` to `CLOUD_PROVIDER_OPENSTACK_CHART_REPO` since it now serves two charts.

**Not verified:** everything on a live cluster. Open items recorded in the runbook: pod scheduling relative to both prerequisites, the provision/attach/mount/expand round trip, the `/etc/cacert` hostPath mount and `kubeletDir` behavior on Talos, and the missing-Secret failure mode.

**How to apply:** when bumping the chart version, re-run the ignored real-chart test `helm::tests::openstack_cinder_csi_chart_renders_the_shape_the_spec_relies_on` and re-check the Secret mount, the StorageClass shape and the controller-plugin's hostNetwork/tolerations.
```

In `docs/memory/MEMORY.md`, append one line:

```markdown
- [CsiDriver (OpenStack Cinder) slice](csi-driver-openstack-cinder-2026-09.md) — fourth CRD, not a singleton (one CR per driver); chart 2.36.5 facts (no Secret rendered, controller-plugin has no hostNetwork/tolerations); nothing live-verified yet
```

- [ ] **Step 5: Add the RBAC ledger rows**

In `docs/memory/rbac-cluster-admin-tradeoff.md`, add these rows directly after the row that begins `| \`""\` (core), \`apps\`, \`rbac.authorization.k8s.io\` | \`serviceaccounts\`, \`daemonsets\` in \`kube-system\`; \`clusterroles\`, \`clusterrolebindings\`, \`roles\`, \`rolebindings\` | OpenStack CCM chart`:

```markdown
| `platform.rye.ninja` | `csidrivers`, `csidrivers/status` | Controller itself | Its own fourth CRD (2026-09-28) — get/list/watch/update/patch, same as `cniinstallations`; finalizer updates go through `update`/`patch` on the main resource. |
| `""` (core), `apps`, `storage.k8s.io` | `serviceaccounts`, `events`, `persistentvolumes`, `persistentvolumeclaims`, `persistentvolumeclaims/status`, `pods`, `nodes` in core; `deployments`, `daemonsets` in `apps`; `storageclasses`, `csinodes`, `csidrivers`, `volumeattachments`, `volumeattachments/status` in `storage.k8s.io` | openstack-cinder-csi chart (`2.36.5`, rendered with `--no-hooks`) | Applies no *kinds* the controller does not already apply, except `storage.k8s.io/storageclasses` and `storage.k8s.io/csidrivers` (new: the CniInstallation/CCM charts never touched storage.k8s.io). The chart's five ClusterRoles (attacher, provisioner, snapshotter, resizer, nodeplugin) also grant `snapshot.storage.k8s.io` `volumesnapshots`/`volumesnapshotclasses`/`volumesnapshotcontents` (get/list, and full CRUD for the snapshotter role) and `coordination.k8s.io` `leases` (the external sidecars' own leader election, separate from this controller's own Lease); because of privilege-escalation prevention the controller must hold every one of those to create the ClusterRoles (currently guaranteed by `cluster-admin`). No cluster-wide `snapshot-controller` or `VolumeSnapshotClass` CRDs are installed by this chart or this controller. |
```

- [ ] **Step 6: Commit**

```bash
git add tests/integration_csi_driver.rs docs/runbooks/csi-driver-openstack-cinder-verification.md docs/memory/csi-driver-openstack-cinder-2026-09.md docs/memory/MEMORY.md docs/memory/rbac-cluster-admin-tradeoff.md
git commit -m "$(cat <<'EOF'
docs: CsiDriver runbook, integration test and memory

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7: Full verification**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | grep -E "^test result|FAILED|failed" ; cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | sort | uniq -c`
Expected: build succeeds; every `test result` line is `ok` with 0 failed (ignored tests are skipped); clippy reports only warning classes the existing code already triggers (`result_large_err`, `too_many_arguments`, and the pre-existing ones in `apply.rs` and `manifests.rs`), no errors.

Then run the real-chart tests, which need network and `helm`: `cargo test --lib -- --ignored openstack 2>&1 | tail -15`. Expected: PASS (both the CCM and the CSI driver real-chart tests).

---

## Self-Review

**Spec coverage.**
- API and validation (chartVersion empty/whitespace, Secret name, helmValues object, `secret.data` rejection, name-must-match-driver, `platformKind`, `driver` via serde, `defaultStorageClass`): Task 1 and Task 3.
- Values layering, Secret passthrough (`enabled`/`create`/`hostMount`/`name`), `defaultStorageClass`'s three cases setting both `isDefault` flags: Task 1.
- Reconcile flow, no synthesized namespace, ledger checkpoint, prune, `Ready`, 300s requeue: Task 3.
- No `dnsPolicy`/`extraVolumes` override and why (no CCM-style deadlock): Global Constraints and the real-chart test comments in Task 2.
- Missing Secret is not checked and is documented: Task 5 (README, example) and Task 6 (runbook step 6).
- Ordering (controller-plugin needs CNI + CCM, no toleration added): Task 2 (real-chart test), Task 5 (README, example), Task 6 (runbook step 1), memory entry.
- Cleanup semantics (reverse ledger order, no waits, volumes not deleted): Task 3 (code comments), Task 5 (README), Task 6 (runbook step 7).
- Not-a-singleton shape, name-must-match-driver, `Driver::expected_name`: Task 1 and Task 3.
- Future providers table: not code — carried in the spec itself; no task claims to implement it, matching "None of these rows adds Rust code in this slice."
- Wiring (fourth watcher and controller, same lease, predicate filter): Task 4.
- Code layout, CRD file regeneration, README, example, runbook, memory, RBAC ledger rows: Tasks 4 to 6.
- Testing (unit, real-chart ignored, example, integration ignored, live runbook): Tasks 1 to 3, 5 and 6.
- Shared `SecretNameRef`/DNS-1123 validator move: Task 1, Step 1.

**Placeholder scan.** None. The runbook's "Findings to record" section lists what to capture from a run that has not happened; it is not a deferred implementation step.

**Type consistency.** `OpenstackCinderSpec`, `SecretNameRef` (now in `crd.rs`), `Driver`, `DefaultStorageClass`, `CsiDriverSpec`/`Status`, `CsiSpecError` (variants `EmptyChartVersion`, `ChartVersionHasWhitespace`, `InvalidSecretName`, `HelmValuesNotObject`, `SecretDataInHelmValues`), `validate_openstack_cinder`, `build_values` (Task 1) match their uses in Tasks 2 to 5. `OPENSTACK_CINDER_CSI_CHART` and `CINDER_CSI_NAMESPACE` (Task 2) match Task 3. `ValidationError` (with its `UnsupportedName { given, driver, expected }` shape), `CsiReconcileError`, `validate`, `reconcile_with_finalizer`, `error_policy` (Task 3) match Tasks 4 and 5. The DaemonSet name `openstack-cinder-csi-nodeplugin` and Deployment name `openstack-cinder-csi-controllerplugin` match across Tasks 2, 5 and 6.
