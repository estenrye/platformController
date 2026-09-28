# CsiDriver Typed StorageClass Customization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `CsiDriver`'s `openstackCinder` spec typed `parameters` on its two built-in StorageClasses (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`) and a typed list of additional StorageClasses, closing the gap live-testing found (a Nova/Cinder availability-zone mismatch with no typed fix).

**Architecture:** `OpenstackCinderSpec`'s bare `defaultStorageClass` field is replaced by a nested `storageClasses` block (`default`, `delete.parameters`, `retain.parameters`, `additional[]`). `build_values` renders each `additional[]` entry into a real `StorageClass` YAML document and joins them into the chart's own `storageClass.custom` raw-YAML extension point — no new render path, no reconciler changes. Four new validation rejections keep the "exactly one default StorageClass" invariant and reject unusable custom names before anything is applied.

**Tech Stack:** Rust (edition 2024), `serde`/`serde_json`/`serde_yaml` (already dependencies), the `openstack-cinder-csi` Helm chart's `storageClass.custom` values field (unchanged chart version, `2.36.5`).

**Spec:** [docs/superpowers/specs/2026-09-28-csi-driver-storage-class-customization-design.md](../specs/2026-09-28-csi-driver-storage-class-customization-design.md)

## Global Constraints

Every task's requirements implicitly include this section. Values are copied verbatim from the spec.

- `OpenstackCinderSpec.defaultStorageClass` (a bare field) is **replaced**, not extended, by a nested `storageClasses` block. This is a breaking reshape of the CR's spec shape, accepted because the CRD is hours old with exactly one live CR.
- New shape: `storageClasses.default` (enum `delete | retain | none`, same three values, new location, defaults to `delete`), `storageClasses.delete.parameters` / `.retain.parameters` (`map[string]string`, default empty), `storageClasses.additional[]` (list, default empty) of `{ name: String, reclaimPolicy: Delete | Retain, parameters: map[string]string, isDefault: bool }`.
- `reclaimPolicy`'s two values are spelled `Delete`/`Retain` (capitalized) — unlike this CRD's other enums, because the value is copied verbatim into the rendered `StorageClass`'s own `reclaimPolicy` field, which the Kubernetes API itself spells that way.
- `storageClasses.default` does **not** grow a fourth value naming a custom entry. An additional StorageClass becomes the cluster default through its own `isDefault: true`, never through `default`.
- Validation, each rejected with `phase: Failed` and a `reason` on `CsiSpecError`:
  - an `additional[].name` is empty or not a valid Kubernetes object name (DNS-1123 subdomain, reuse `crate::crd::is_dns1123_subdomain`) — reason `InvalidStorageClassName`
  - an `additional[].name` equals `csi-cinder-sc-delete` or `csi-cinder-sc-retain` (the chart's own built-in names) — reason `ReservedStorageClassName`
  - two `additional[]` entries share the same `name` — reason `DuplicateStorageClassName`
  - `storageClasses.default != none` and any `additional[].isDefault == true`, **or** more than one `additional[].isDefault == true` — reason `AmbiguousDefaultStorageClass`
- `build_values` sets `storageClass.delete.parameters` / `storageClass.retain.parameters` from the typed maps (empty map, not omitted, when unset) and `storageClass.custom` from `additional[]` — **unconditionally**, even when `additional[]` is empty (an empty string, not an absent key), exactly like every other typed field in this file, so a `helmValues.storageClass.custom` passthrough is always overwritten. No new rejection for that case — it's the same "typed always wins" behavior already covering every other typed/`helmValues` conflict in this file.
- Each `additional[]` entry renders to this exact document shape, joined with `---\n` between entries:
  ```yaml
  apiVersion: storage.k8s.io/v1
  kind: StorageClass
  metadata:
    name: <entry.name>
    annotations:                                              # only when entry.isDefault
      storageclass.kubernetes.io/is-default-class: "true"
  provisioner: cinder.csi.openstack.org
  reclaimPolicy: <entry.reclaimPolicy>
  parameters: <entry.parameters>
  ```
- No changes to `csi_reconciler.rs`, the reconcile loop, cleanup, or the finalizer — this plan only changes what feeds the existing `helm template` call.
- CI runs `cargo build`, `cargo test`, `cargo clippy --all-targets`; same pre-accepted warning classes as before (`result_large_err`, `too_many_arguments`, the two pre-existing ones in `apply.rs`/`manifests.rs`).
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- CLAUDE.md: project memory lives in `docs/memory/` (index in `docs/memory/MEMORY.md`), committed like any other change. Do not write to the out-of-repo memory path.

## Review Focus

Failure modes the spec implies but that are easiest to miss. Each has a test in the task that owns the code.

1. An `additional[]` entry named `csi-cinder-sc-delete` or `csi-cinder-sc-retain` must be rejected before render — Kubernetes would otherwise get two StorageClass objects (one chart-rendered, one from `storageClass.custom`) fighting over the same name inside one `helm template` output. Test in Task 1.
2. Two StorageClasses both claiming `isDefault: true` (via `default` + an `additional[]` entry, or two `additional[]` entries) must be rejected — Kubernetes itself allows multiple default StorageClasses to exist simultaneously (it just makes PVC binding unpredictable), so nothing downstream catches this if the controller doesn't. Test in Task 1.
3. An empty `additional[]` list must still set `storageClass.custom` to an explicit empty string, not omit the key — otherwise a `helmValues.storageClass.custom` a user set before this feature existed would silently keep taking effect instead of being cleanly superseded. Test in Task 1.
4. An unset `parameters` map must render as `{}`, not `null` or an absent key — the chart's own default is `{}`, and passing `null` through Helm's values merge can behave differently from an empty map for some chart templates. Test in Task 1.
5. A `StorageClass` document generated from an `additional[]` entry must actually be accepted by the real chart's template concatenation, not just look correct as a standalone YAML string — `serde_yaml`'s exact indentation/quoting choices are untested against the live `helm template` call anywhere else in this codebase. Test in Task 2 (real-chart, ignored).

---

## File Structure

| File | Responsibility |
|---|---|
| `src/csi_driver.rs` (modify) | `OpenstackCinderSpec` reshaped; new `StorageClassesSpec`/`BuiltinStorageClassSpec`/`AdditionalStorageClassSpec`/`ReclaimPolicy` types; 4 new `CsiSpecError` variants; `validate_openstack_cinder` and `build_values` extended. |
| `deploy/crd.yaml` (regenerate) | The `CsiDriver` CRD's OpenAPI schema changes shape along with `OpenstackCinderSpec`; this file is checked into git and diffed against the generator by an existing test, so it must be regenerated in the same task. |
| `src/helm.rs` (modify) | Real-chart test extended to render one `additional[]` entry through the live chart. |
| `examples/csi-driver-openstack-cinder.yaml` (modify) | `defaultStorageClass: delete` moves to `storageClasses.default: delete`. |
| `tests/csi_driver_example.rs` (modify) | One new test asserting the example's default StorageClass. |
| `deploy/README.md` (modify) | `spec.openstackCinder.defaultStorageClass` reference updated to the new path. |
| `docs/runbooks/csi-driver-openstack-cinder-verification.md` (modify) | Same field-path update. |
| `docs/memory/csi-driver-openstack-cinder-2026-09.md`, `docs/memory/MEMORY.md` | Memory entry addition and index line. |

---

### Task 1: Reshape the spec, add validation and extend the values builder

**Files:**
- Modify: `src/csi_driver.rs`
- Regenerate: `deploy/crd.yaml`
- Test: inline `#[cfg(test)]` module in `src/csi_driver.rs`; `tests/bootstrap_manifests.rs`'s existing `crd_yaml_matches_the_generated_crds` (unmodified, but only passes once `deploy/crd.yaml` is regenerated)

**Interfaces:**
- Consumes: `crate::crd::is_dns1123_subdomain` (existing); `crate::pull_through_cache::merge` (existing).
- Produces:
  - `OpenstackCinderSpec { chart_version, cloud_config_secret_ref, storage_classes: StorageClassesSpec, helm_values }` (the `default_storage_class` field is removed)
  - `StorageClassesSpec { default: DefaultStorageClass, delete: BuiltinStorageClassSpec, retain: BuiltinStorageClassSpec, additional: Vec<AdditionalStorageClassSpec> }` (derives `Default`)
  - `BuiltinStorageClassSpec { parameters: std::collections::BTreeMap<String, String> }` (derives `Default`)
  - `AdditionalStorageClassSpec { name: String, reclaim_policy: ReclaimPolicy, parameters: std::collections::BTreeMap<String, String>, is_default: bool }`
  - `ReclaimPolicy { Delete, Retain }`
  - `CsiSpecError` gains `InvalidStorageClassName(String)`, `ReservedStorageClassName(String)`, `DuplicateStorageClassName(String)`, `AmbiguousDefaultStorageClass(String)`, each mapped by `reason()` to the reason strings in Global Constraints
  - `validate_openstack_cinder`, `build_values`: same signatures as today, extended behavior

- [ ] **Step 1: Write the failing tests**

Add the following tests to the `#[cfg(test)] mod tests` block in `src/csi_driver.rs` (after the existing `rejects_secret_names_kubernetes_rejects` test, before `rejects_helm_values_that_are_not_an_object`), and update the seven tests listed in Step 2 below. Do not implement anything yet — these reference types and fields that don't exist yet, so the crate won't compile; that's expected.

```rust
    #[test]
    fn additional_storage_class_names_must_be_valid_dns1123_names() {
        let mut spec = openstack_cinder();
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "Not_Valid".to_string(),
            reclaim_policy: ReclaimPolicy::Delete,
            parameters: Default::default(),
            is_default: false,
        });

        let err = validate_openstack_cinder(&spec).expect_err("invalid StorageClass name is invalid");

        assert_eq!(err, CsiSpecError::InvalidStorageClassName("Not_Valid".to_string()));
        assert_eq!(err.reason(), "InvalidStorageClassName");
    }

    #[test]
    fn additional_storage_class_names_cannot_collide_with_the_reserved_builtin_names() {
        for reserved in ["csi-cinder-sc-delete", "csi-cinder-sc-retain"] {
            let mut spec = openstack_cinder();
            spec.storage_classes.additional.push(AdditionalStorageClassSpec {
                name: reserved.to_string(),
                reclaim_policy: ReclaimPolicy::Delete,
                parameters: Default::default(),
                is_default: false,
            });

            let err = validate_openstack_cinder(&spec).expect_err("reserved name is invalid");

            assert_eq!(err, CsiSpecError::ReservedStorageClassName(reserved.to_string()), "{reserved}");
            assert_eq!(err.reason(), "ReservedStorageClassName");
        }
    }

    #[test]
    fn additional_storage_class_names_must_be_unique() {
        let mut spec = openstack_cinder();
        for _ in 0..2 {
            spec.storage_classes.additional.push(AdditionalStorageClassSpec {
                name: "csi-cinder-sc-az1".to_string(),
                reclaim_policy: ReclaimPolicy::Delete,
                parameters: Default::default(),
                is_default: false,
            });
        }

        let err = validate_openstack_cinder(&spec).expect_err("duplicate name is invalid");

        assert_eq!(err, CsiSpecError::DuplicateStorageClassName("csi-cinder-sc-az1".to_string()));
        assert_eq!(err.reason(), "DuplicateStorageClassName");
    }

    #[test]
    fn default_storage_class_cannot_conflict_with_an_additional_default() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::Delete;
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "csi-cinder-sc-az1".to_string(),
            reclaim_policy: ReclaimPolicy::Delete,
            parameters: Default::default(),
            is_default: true,
        });

        let err = validate_openstack_cinder(&spec).expect_err("two defaults is invalid");

        assert_eq!(err.reason(), "AmbiguousDefaultStorageClass");
    }

    #[test]
    fn at_most_one_additional_entry_can_be_the_default() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::None;
        for name in ["csi-cinder-sc-az1", "csi-cinder-sc-az2"] {
            spec.storage_classes.additional.push(AdditionalStorageClassSpec {
                name: name.to_string(),
                reclaim_policy: ReclaimPolicy::Delete,
                parameters: Default::default(),
                is_default: true,
            });
        }

        let err = validate_openstack_cinder(&spec).expect_err("two additional defaults is invalid");

        assert_eq!(err.reason(), "AmbiguousDefaultStorageClass");
    }

    #[test]
    fn accepts_a_spec_with_no_conflicting_defaults() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::None;
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "csi-cinder-sc-az1".to_string(),
            reclaim_policy: ReclaimPolicy::Delete,
            parameters: Default::default(),
            is_default: true,
        });

        assert_eq!(validate_openstack_cinder(&spec), Ok(()));
    }

    #[test]
    fn values_set_typed_parameters_on_the_builtin_storage_classes() {
        let mut spec = openstack_cinder();
        spec.storage_classes.delete.parameters.insert("availability".to_string(), "nova".to_string());
        spec.storage_classes.retain.parameters.insert("type".to_string(), "fast".to_string());

        let values = build_values(&spec);

        assert_eq!(values["storageClass"]["delete"]["parameters"]["availability"], "nova");
        assert_eq!(values["storageClass"]["retain"]["parameters"]["type"], "fast");
    }

    #[test]
    fn values_default_parameters_render_as_an_empty_object_not_null() {
        let values = build_values(&openstack_cinder());

        assert_eq!(values["storageClass"]["delete"]["parameters"], serde_json::json!({}));
        assert_eq!(values["storageClass"]["retain"]["parameters"], serde_json::json!({}));
    }

    #[test]
    fn values_storage_class_custom_is_an_empty_string_when_no_additional_entries() {
        let values = build_values(&openstack_cinder());

        assert_eq!(values["storageClass"]["custom"], "");
    }

    #[test]
    fn values_render_additional_storage_classes_into_storage_class_custom() {
        use serde::Deserialize;

        let mut spec = openstack_cinder();
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "csi-cinder-sc-az1".to_string(),
            reclaim_policy: ReclaimPolicy::Retain,
            parameters: std::collections::BTreeMap::from([("availability".to_string(), "az1".to_string())]),
            is_default: false,
        });

        let values = build_values(&spec);
        let custom = values["storageClass"]["custom"].as_str().expect("custom is a string");
        let docs: Vec<serde_json::Value> = serde_yaml::Deserializer::from_str(custom)
            .map(|doc| serde_yaml::Value::deserialize(doc).expect("valid YAML"))
            .map(|v| serde_json::to_value(v).expect("YAML converts to JSON"))
            .collect();

        assert_eq!(docs.len(), 1, "{custom}");
        assert_eq!(docs[0]["apiVersion"], "storage.k8s.io/v1");
        assert_eq!(docs[0]["kind"], "StorageClass");
        assert_eq!(docs[0]["metadata"]["name"], "csi-cinder-sc-az1");
        assert_eq!(docs[0]["provisioner"], "cinder.csi.openstack.org");
        assert_eq!(docs[0]["reclaimPolicy"], "Retain");
        assert_eq!(docs[0]["parameters"]["availability"], "az1");
        assert!(docs[0]["metadata"]["annotations"].is_null(), "{custom}");
    }

    #[test]
    fn values_mark_an_additional_storage_class_default_via_annotation() {
        // serde_yaml quotes an ambiguous scalar like the string "true" (as
        // 'true', not "true", and the exact quote style is an implementation
        // detail) to keep it a string on re-parse, so this parses the
        // generated YAML back rather than substring-matching a specific
        // quote style.
        use serde::Deserialize;

        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::None;
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "csi-cinder-sc-az1".to_string(),
            reclaim_policy: ReclaimPolicy::Delete,
            parameters: Default::default(),
            is_default: true,
        });

        let values = build_values(&spec);
        let custom = values["storageClass"]["custom"].as_str().expect("custom is a string");
        let doc = serde_yaml::Value::deserialize(serde_yaml::Deserializer::from_str(custom).next().unwrap())
            .expect("valid YAML");
        let annotation = &doc["metadata"]["annotations"]["storageclass.kubernetes.io/is-default-class"];

        assert_eq!(annotation.as_str(), Some("true"), "{custom}");
    }

    #[test]
    fn typed_storage_class_custom_always_overrides_a_helm_values_attempt() {
        let mut spec = openstack_cinder();
        spec.helm_values = Some(serde_json::json!({
            "storageClass": { "custom": "kind: StorageClass\nmetadata:\n  name: sneaky\n" }
        }));

        let values = build_values(&spec);

        assert_eq!(values["storageClass"]["custom"], "");
    }
```

- [ ] **Step 2: Update the seven existing tests that reference the old field**

In the same file, in `spec_deserializes_with_only_the_required_fields`, replace:

```rust
        assert_eq!(spec.openstack_cinder.default_storage_class, DefaultStorageClass::Delete);
```

with:

```rust
        assert_eq!(spec.openstack_cinder.storage_classes.default, DefaultStorageClass::Delete);
        assert!(spec.openstack_cinder.storage_classes.delete.parameters.is_empty());
        assert!(spec.openstack_cinder.storage_classes.retain.parameters.is_empty());
        assert!(spec.openstack_cinder.storage_classes.additional.is_empty());
```

In `default_storage_class_delete_marks_the_delete_class_default`, `default_storage_class_retain_marks_the_retain_class_default` and `default_storage_class_none_marks_neither_class_default`, and in the three `typed_values_win_over_conflicting_helm_values_default_storage_class_*` tests, replace every occurrence of:

```rust
        spec.default_storage_class = DefaultStorageClass::
```

with:

```rust
        spec.storage_classes.default = DefaultStorageClass::
```

(six occurrences total across those six tests — the variant name after `DefaultStorageClass::` on each line is unchanged).

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib csi_driver:: 2>&1 | tail -30`
Expected: FAIL to compile — `storage_classes`, `StorageClassesSpec`, `AdditionalStorageClassSpec`, `ReclaimPolicy` and the four new `CsiSpecError` variants don't exist yet.

- [ ] **Step 4: Write the implementation**

Replace the entire contents of `src/csi_driver.rs` above the `#[cfg(test)]` line with the following (the test module from Steps 1 and 2 follows unchanged after it):

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind, SecretNameRef};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Which StorageClass is the cluster's default, plus typed customization
    /// of the chart's own two StorageClasses and any number of additional
    /// ones.
    #[serde(default)]
    pub storage_classes: StorageClassesSpec,
    /// Free-form values merged into the chart's values. Typed fields are
    /// overlaid on top, so they always win.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::pull_through_cache::preserve_unknown_object")]
    pub helm_values: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageClassesSpec {
    /// Which of the chart's two built-in StorageClasses is the cluster
    /// default. The controller always sets both `storageClass.*.isDefault`
    /// flags from this one field, so a `helmValues` passthrough can never
    /// create two defaults between them. Making an additional StorageClass
    /// the default is done through that entry's own `isDefault`, not here.
    #[serde(default)]
    pub default: DefaultStorageClass,
    /// Typed `parameters` for `csi-cinder-sc-delete`.
    #[serde(default)]
    pub delete: BuiltinStorageClassSpec,
    /// Typed `parameters` for `csi-cinder-sc-retain`.
    #[serde(default)]
    pub retain: BuiltinStorageClassSpec,
    /// Additional StorageClasses beyond the chart's built-in delete/retain
    /// pair. Rendered into the chart's `storageClass.custom` raw-YAML
    /// extension point, unconditionally, so this list is always the single
    /// source of truth for extra StorageClasses once set.
    #[serde(default)]
    pub additional: Vec<AdditionalStorageClassSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinStorageClassSpec {
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalStorageClassSpec {
    /// Must be a valid Kubernetes object name and must not collide with the
    /// chart's own `csi-cinder-sc-delete`/`csi-cinder-sc-retain` names.
    pub name: String,
    pub reclaim_policy: ReclaimPolicy,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// At most one StorageClass total (this pool plus `storageClasses.default`)
    /// may set this to `true`.
    #[serde(default)]
    pub is_default: bool,
}

/// Spelled `Delete`/`Retain` (capitalized), unlike this CRD's other enums:
/// the value is copied verbatim into the rendered `StorageClass`'s own
/// `reclaimPolicy` field, which the Kubernetes API itself spells that way.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub enum ReclaimPolicy {
    Delete,
    Retain,
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
    #[error(
        "spec.openstackCinder.storageClasses.additional[].name {0:?} is not a valid Kubernetes \
         object name (lowercase alphanumerics, '-' and '.', starting and ending with an \
         alphanumeric, at most 253 characters)"
    )]
    InvalidStorageClassName(String),
    #[error(
        "spec.openstackCinder.storageClasses.additional[].name {0:?} is reserved: it collides \
         with one of the chart's built-in StorageClass names (csi-cinder-sc-delete, \
         csi-cinder-sc-retain)"
    )]
    ReservedStorageClassName(String),
    #[error("spec.openstackCinder.storageClasses.additional contains the name {0:?} more than once")]
    DuplicateStorageClassName(String),
    #[error("spec.openstackCinder.storageClasses has more than one default StorageClass: {0}")]
    AmbiguousDefaultStorageClass(String),
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
            CsiSpecError::InvalidStorageClassName(_) => "InvalidStorageClassName",
            CsiSpecError::ReservedStorageClassName(_) => "ReservedStorageClassName",
            CsiSpecError::DuplicateStorageClassName(_) => "DuplicateStorageClassName",
            CsiSpecError::AmbiguousDefaultStorageClass(_) => "AmbiguousDefaultStorageClass",
        }
    }
}

const RESERVED_STORAGE_CLASS_NAMES: [&str; 2] = ["csi-cinder-sc-delete", "csi-cinder-sc-retain"];

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

    let mut seen_names = std::collections::HashSet::new();
    for entry in &spec.storage_classes.additional {
        if !crate::crd::is_dns1123_subdomain(&entry.name) {
            return Err(CsiSpecError::InvalidStorageClassName(entry.name.clone()));
        }
        if RESERVED_STORAGE_CLASS_NAMES.contains(&entry.name.as_str()) {
            return Err(CsiSpecError::ReservedStorageClassName(entry.name.clone()));
        }
        if !seen_names.insert(entry.name.as_str()) {
            return Err(CsiSpecError::DuplicateStorageClassName(entry.name.clone()));
        }
    }

    let additional_default_count =
        spec.storage_classes.additional.iter().filter(|entry| entry.is_default).count();
    if spec.storage_classes.default != DefaultStorageClass::None && additional_default_count > 0 {
        return Err(CsiSpecError::AmbiguousDefaultStorageClass(format!(
            "storageClasses.default is {:?} and at least one storageClasses.additional[] entry \
             also has isDefault: true",
            spec.storage_classes.default
        )));
    }
    if additional_default_count > 1 {
        return Err(CsiSpecError::AmbiguousDefaultStorageClass(
            "more than one storageClasses.additional[] entry has isDefault: true".to_string(),
        ));
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

/// Renders `storageClasses.additional` into the chart's `storageClass.custom`
/// raw-YAML extension point: one `StorageClass` document per entry, joined by
/// `---\n`. An empty list produces an empty string, not an absent value.
fn render_additional_storage_classes(additional: &[AdditionalStorageClassSpec]) -> String {
    additional
        .iter()
        .map(|entry| {
            let mut metadata = serde_json::json!({ "name": entry.name });
            if entry.is_default {
                metadata["annotations"] = serde_json::json!({
                    "storageclass.kubernetes.io/is-default-class": "true",
                });
            }
            let document = serde_json::json!({
                "apiVersion": "storage.k8s.io/v1",
                "kind": "StorageClass",
                "metadata": metadata,
                "provisioner": "cinder.csi.openstack.org",
                "reclaimPolicy": entry.reclaim_policy,
                "parameters": entry.parameters,
            });
            serde_yaml::to_string(&document).expect("StorageClass JSON always serializes to YAML")
        })
        .collect::<Vec<_>>()
        .join("---\n")
}

/// The Helm values for the openstack-cinder-csi chart: the user's
/// `helmValues` passthrough, with the typed fields overlaid on top.
///
/// - The Secret is user-created, so the chart must use it, never create one,
///   and never fall back to the host-path `/etc/cloud/cloud.conf` the chart
///   would otherwise read by default (`secret.hostMount`).
/// - `storageClasses.default` deterministically sets both built-in
///   StorageClasses' `isDefault` flags, so at most one of this chart's own
///   two StorageClasses is ever the cluster default.
/// - `storageClasses.additional` is rendered into `storageClass.custom`
///   unconditionally, so it is always the single source of truth for extra
///   StorageClasses.
pub fn build_values(spec: &OpenstackCinderSpec) -> serde_json::Value {
    let mut values = spec.helm_values.clone().unwrap_or_else(|| serde_json::json!({}));

    let (delete_is_default, retain_is_default) = match spec.storage_classes.default {
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
            "delete": {
                "isDefault": delete_is_default,
                "parameters": spec.storage_classes.delete.parameters,
            },
            "retain": {
                "isDefault": retain_is_default,
                "parameters": spec.storage_classes.retain.parameters,
            },
            "custom": render_additional_storage_classes(&spec.storage_classes.additional),
        },
        // The chart's default csi.plugin.volumes hostPath-mounts /etc/cacert
        // (an optional TLS CA bundle) on both the node and controller plugin
        // containers. Talos's root filesystem is read-only and never creates
        // that directory, so the container runtime's mkdir for the bind mount
        // fails outright ("failed to mkdir \"/etc/cacert\": read-only file
        // system") -- live-verified against a real Talos cluster. The only
        // other consumer of csi.plugin.volumeMounts is the required
        // cloud-config Secret mount, so it's replaced here rather than
        // emptied, unconditionally so a helmValues passthrough can never
        // reintroduce the crash.
        "csi": {
            "plugin": {
                "volumes": [],
                "volumeMounts": [
                    { "name": "cloud-config", "mountPath": "/etc/config", "readOnly": true },
                ],
            },
        },
    });

    crate::pull_through_cache::merge(&mut values, typed);
    values
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib csi_driver:: 2>&1 | tail -40`
Expected: PASS (35 tests: the 23 that existed before this task, plus the 12 new ones from Step 1).
Run: `cargo build 2>&1 | tail -10`
Expected: builds.

- [ ] **Step 6: Regenerate the CRD**

`OpenstackCinderSpec`'s reshape changes the `CsiDriver` CRD's generated OpenAPI schema. Regenerate the checked-in file:

```bash
cargo run -q --bin crdgen > deploy/crd.yaml
```

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -10`
Expected: PASS (5 tests, including `crd_yaml_matches_the_generated_crds`, which would otherwise fail — that test diffs this file against the live generator).

- [ ] **Step 7: Confirm the existing real-chart test still passes unmodified**

This task doesn't change Task 2's real-chart test, but `build_values` now always sets `storageClass.{delete,retain}.parameters` (empty maps) and `storageClass.custom` (an empty string) where neither key was set before. Confirm the live chart treats an explicit empty string the same as an absent key (Helm/Go template truthiness: both are falsy) before relying on that in Task 2:

Run: `cargo test --lib -- --ignored openstack_cinder_csi 2>&1 | tail -15`
Expected: PASS, unchanged from before this task (needs network access and the `helm` CLI) — confirms the new unconditional keys don't perturb the chart's rendered output on the no-`additional` path.

- [ ] **Step 8: Commit**

```bash
git add src/csi_driver.rs deploy/crd.yaml
git commit -m "$(cat <<'EOF'
feat: typed StorageClass parameters and additional StorageClasses

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Extend the real-chart test for additional StorageClasses

**Files:**
- Modify: `src/helm.rs`

**Interfaces:**
- Consumes: `crate::csi_driver::{OpenstackCinderSpec, AdditionalStorageClassSpec, ReclaimPolicy, StorageClassesSpec, build_values}` (Task 1).
- Produces: nothing later tasks call.

- [ ] **Step 1: Update the real-chart test**

In `src/helm.rs`, inside `openstack_cinder_csi_chart_renders_the_shape_the_spec_relies_on`, replace the spec construction:

```rust
        let openstack_cinder = crate::csi_driver::OpenstackCinderSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: crate::crd::SecretNameRef {
                name: "my-cloud-config".to_string(),
            },
            ..Default::default()
        };
```

with:

```rust
        let openstack_cinder = crate::csi_driver::OpenstackCinderSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: crate::crd::SecretNameRef {
                name: "my-cloud-config".to_string(),
            },
            storage_classes: crate::csi_driver::StorageClassesSpec {
                additional: vec![crate::csi_driver::AdditionalStorageClassSpec {
                    name: "csi-cinder-sc-az1".to_string(),
                    reclaim_policy: crate::csi_driver::ReclaimPolicy::Delete,
                    parameters: std::collections::BTreeMap::from([(
                        "availability".to_string(),
                        "az1".to_string(),
                    )]),
                    is_default: false,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
```

Then, after the existing `retain_class` assertions (the block ending with the `is_none()` assertion on `retain_class`'s annotations) and before the `// --no-hooks:` comment at the end of the function, add:

```rust
        // storageClasses.additional renders a real extra StorageClass through
        // the chart's own storageClass.custom raw-YAML extension point.
        let extra_class = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-az1"))
            .expect("the additional StorageClass");
        assert_eq!(extra_class.types.as_ref().unwrap().kind, "StorageClass");
        assert_eq!(extra_class.data["provisioner"], "cinder.csi.openstack.org");
        assert_eq!(extra_class.data["reclaimPolicy"], "Delete");
        assert_eq!(extra_class.data["parameters"]["availability"], "az1");
```

- [ ] **Step 2: Run it against the real chart**

Run: `cargo test --lib -- --ignored openstack_cinder_csi 2>&1 | tail -20`
Expected: PASS (needs network access and the `helm` CLI).

- [ ] **Step 3: Commit**

```bash
git add src/helm.rs
git commit -m "$(cat <<'EOF'
test: exercise storageClasses.additional against the real chart

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Update the example, its test, and docs

**Files:**
- Modify: `examples/csi-driver-openstack-cinder.yaml`, `tests/csi_driver_example.rs`, `deploy/README.md`, `docs/runbooks/csi-driver-openstack-cinder-verification.md`
- Create: `docs/memory/csi-driver-openstack-cinder-2026-09.md` gains a new section (not a new file); `docs/memory/MEMORY.md` gains no new line (same memory file, description unchanged — this task edits the existing entry's body only, see Step 4)

**Interfaces:**
- Consumes: `csi_driver::{StorageClassesSpec, DefaultStorageClass}` (Task 1); `csi_reconciler::validate` (existing, unchanged signature).
- Produces: nothing later tasks use.

- [ ] **Step 1: Write the failing test**

Add to `tests/csi_driver_example.rs`, after `example_values_name_the_secret_the_comment_tells_you_to_create`:

```rust
#[test]
fn example_marks_csi_cinder_sc_delete_as_the_cluster_default() {
    use platform_controller::csi_driver::DefaultStorageClass;

    let openstack_cinder = load().spec.openstack_cinder;

    assert_eq!(openstack_cinder.storage_classes.default, DefaultStorageClass::Delete);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test csi_driver_example 2>&1 | tail -20`
Expected: FAIL to compile — the example YAML still has the old `defaultStorageClass` field, so `serde_yaml::from_str` in `load()` either errors (unknown field, if the schema rejects unknown fields) or the new field path doesn't exist on the deserialized value yet. Either way this test cannot pass against the unmodified example.

- [ ] **Step 3: Update the example**

In `examples/csi-driver-openstack-cinder.yaml`, replace:

```yaml
    cloudConfigSecretRef:
      name: cloud-config
    # csi-cinder-sc-delete is the cluster's default StorageClass. Set to
    # "retain" or "none" to choose differently.
    defaultStorageClass: delete
```

with:

```yaml
    cloudConfigSecretRef:
      name: cloud-config
    storageClasses:
      # csi-cinder-sc-delete is the cluster's default StorageClass. Set to
      # "retain" or "none" to choose differently. delete/retain parameters
      # (e.g. availability, type) and additional StorageClasses go here too;
      # see docs/superpowers/specs/2026-09-28-csi-driver-storage-class-customization-design.md
      default: delete
```

- [ ] **Step 4: Update the docs**

In `deploy/README.md`, replace:

```markdown
`spec.openstackCinder.chartVersion` is the Helm chart version (`2.36.5`), not
the application version (`v1.36.0`). `spec.openstackCinder.defaultStorageClass`
(`delete`, `retain` or `none`; default `delete`) picks which StorageClass, if
any, is the cluster default.
```

with:

```markdown
`spec.openstackCinder.chartVersion` is the Helm chart version (`2.36.5`), not
the application version (`v1.36.0`). `spec.openstackCinder.storageClasses.default`
(`delete`, `retain` or `none`; default `delete`) picks which of the two
built-in StorageClasses, if any, is the cluster default. Each of
`storageClasses.delete`/`.retain` takes a typed `parameters` map (e.g.
`availability`, `type`), and `storageClasses.additional` is a list of further
StorageClasses (`name`, `reclaimPolicy`, `parameters`, `isDefault`) --
directly motivated by the Nova/Cinder availability-zone mismatch found while
live-testing this driver (see the runbook).
```

In `docs/runbooks/csi-driver-openstack-cinder-verification.md`, replace:

```markdown
`cinder.csi.openstack.org`. With the example's default `defaultStorageClass:
```

with:

```markdown
`cinder.csi.openstack.org`. With the example's default `storageClasses.default:
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test csi_driver_example 2>&1 | tail -20`
Expected: PASS (5 tests).
Run: `cargo test 2>&1 | grep -E "^test result|FAILED"`
Expected: every line `ok`, 0 failed.

- [ ] **Step 6: Add the memory entry**

In `docs/memory/csi-driver-openstack-cinder-2026-09.md`, add this bullet to the "Non-obvious facts" list (after the bullet about `storageClass.delete.isDefault`/`storageClass.retain.isDefault`):

```markdown
- `storageClasses.additional` renders into the chart's own `storageClass.custom` raw-YAML extension point (confirmed from the chart's commented example values, which pairs a custom `StorageClass` and a `VolumeSnapshotClass` in exactly this string form) -- the controller owns generating that string from typed structs, joined with `---\n`, set unconditionally so it's always the single source of truth for extra StorageClasses. `OpenstackCinderSpec.defaultStorageClass` moved to `storageClasses.default` in this same change (a breaking reshape, accepted because the CRD was hours old with one live CR).
```

- [ ] **Step 7: Commit**

```bash
git add examples/csi-driver-openstack-cinder.yaml tests/csi_driver_example.rs deploy/README.md docs/runbooks/csi-driver-openstack-cinder-verification.md docs/memory/csi-driver-openstack-cinder-2026-09.md
git commit -m "$(cat <<'EOF'
docs: update the CsiDriver example and docs for storageClasses

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 8: Full verification**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | grep -E "^test result|FAILED|failed" ; cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | sort | uniq -c`
Expected: build succeeds; every `test result` line is `ok` with 0 failed; clippy reports only the pre-accepted warning classes (`result_large_err`, `too_many_arguments`, the two pre-existing ones in `apply.rs`/`manifests.rs`), no errors.

Then re-run both ignored real-chart tests: `cargo test --lib -- --ignored openstack 2>&1 | tail -15`. Expected: PASS (both the CCM and the CSI driver real-chart tests, the latter now exercising `storageClasses.additional` too).

---

## Self-Review

**Spec coverage.**
- API reshape (`defaultStorageClass` → `storageClasses.default`), typed `parameters` on delete/retain, typed `additional[]`: Task 1.
- Validation (`InvalidStorageClassName`, `ReservedStorageClassName`, `DuplicateStorageClassName`, `AmbiguousDefaultStorageClass` in both directions): Task 1.
- Values-builder mechanics (`storageClass.custom` rendering, unconditional override, empty-list-is-empty-string, default annotation shape): Task 1.
- Real-chart confirmation that a generated `StorageClass` document is accepted by the live chart: Task 2.
- Example/docs updated for the reshaped field: Task 3.
- No reconciler changes: stated in Global Constraints, no task touches `csi_reconciler.rs`.
- `deploy/crd.yaml` regeneration: Task 1, Step 6 — not in the spec itself (an implementation-level consequence of the reshape, caught by re-reading how `deploy/crd.yaml` is tested elsewhere in this codebase), added here rather than left to surface as a surprise test failure.

**Placeholder scan.** None.

**Type consistency.** `StorageClassesSpec`, `BuiltinStorageClassSpec`, `AdditionalStorageClassSpec`, `ReclaimPolicy` (Task 1) match their uses in Task 2's real-chart test and Task 3's example/docs. `CsiSpecError`'s four new variants and their `reason()` mappings (Task 1) match the Global Constraints' reason strings exactly. `render_additional_storage_classes` (private to `csi_driver.rs`, Task 1) is called only from `build_values` in the same task — no cross-task interface for it.
