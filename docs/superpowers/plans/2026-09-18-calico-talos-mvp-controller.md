# Calico-on-Talos MVP Controller Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust Kubernetes controller that reconciles a `CniInstallation` custom resource by rendering the `tigera-operator` Helm chart and applying it via server-side apply, so a freshly bootstrapped Talos Linux cluster with no CNI gets Calico installed declaratively.

**Architecture:** A `kube-rs` `Controller` watches a single cluster-scoped `CniInstallation` CRD. Each reconcile validates the spec, shells out to the `helm` CLI to render the `tigera-operator` chart with translated values, parses the multi-document YAML output into `DynamicObject`s, applies them in dependency order via server-side apply, prunes anything removed since the last reconcile, and records status. The controller itself ships as a `hostNetwork` Deployment with tolerations so it can run before any CNI exists.

**Tech Stack:** Rust, `kube-rs` (client + runtime + derive), `k8s-openapi`, `tokio`, `serde`/`serde_json`/`serde_yaml`, `schemars`, `tempfile`, `chrono`, `thiserror`, `tracing`; `helm` CLI bundled in the container image; `talosctl` for the integration test.

**Spec:** [docs/superpowers/specs/2026-09-18-calico-talos-mvp-controller-design.md](../specs/2026-09-18-calico-talos-mvp-controller-design.md)

## Global Constraints

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `CniInstallation`, cluster-scoped, singleton resource named `default`.
- Only `platformKind: talos-linux` and `provider: calico` are supported; anything else sets `status.phase = Failed` with condition reason `Unsupported` and takes no further action.
- The Calico chart is always fetched live: `helm template calico --repo https://projectcalico.docs.tigera.io/charts tigera-operator --version <spec.calico.chartVersion> --values <file> --include-crds`. No `helm repo add`, no vendored chart.
- All manifests are applied via Kubernetes server-side apply with field manager `platform-controller`, in this exact rank order: `Namespace` (0) → `CustomResourceDefinition` (1) → `ServiceAccount`/`ClusterRole`/`ClusterRoleBinding`/`Role`/`RoleBinding` (2) → `ConfigMap`/`Secret`/`Service`/`ValidatingWebhookConfiguration`/`APIService` (3) → `Deployment`/`DaemonSet` (4) → everything else, including the operator's own custom resources (5).
- The controller's own Deployment must run with `hostNetwork: true`, `dnsPolicy: ClusterFirstWithHostNet`, and tolerations for `node.kubernetes.io/not-ready` (`NoSchedule` and `NoExecute`).
- No leader election, no airgapped chart vendoring, no CNI-readiness polling — "manifests applied" is treated as `Ready` for this MVP (see spec §5 Fast-Follows for what's deliberately deferred).

---

## Task 1: CRD types

**Files:**
- Create: `src/lib.rs`
- Create: `src/crd.rs`
- Modify: `Cargo.toml` (add `schemars` dependency)

**Interfaces:**
- Produces: `platform_controller::crd::{CniInstallation, CniInstallationSpec, CniInstallationStatus, CalicoSpec, CalicoIpPoolSpec, PlatformKind, CniProvider, Encapsulation, Phase, AppliedResourceRef}` — every later task builds on these exact types and field names.

- [ ] **Step 1: Add the schemars dependency**

Run: `cargo add schemars`

- [ ] **Step 2: Write the failing tests**

Create `src/crd.rs` with just the test module first (the types it references don't exist yet, so this won't compile — that's the expected "fails" state for a type-level TDD step):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn spec_round_trips_through_json() {
        let json = serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "calico",
            "calico": {
                "chartVersion": "v3.29.1",
                "bgpEnabled": true,
                "apiServerEnabled": true,
                "ipPools": [{
                    "name": "pods-v6",
                    "cidr": "fd97:45c2:b3a1:1100::/56",
                    "encapsulation": "None",
                    "natOutgoing": true,
                    "blockSize": 122,
                    "nodeSelector": "all()"
                }],
                "nodeAddressAutodetectionV6Cidrs": ["fd97:45c2:b3a1:179::/64"]
            }
        });

        let spec: CniInstallationSpec = serde_json::from_value(json).expect("spec should deserialize");

        assert_eq!(spec.platform_kind, PlatformKind::TalosLinux);
        assert_eq!(spec.provider, CniProvider::Calico);
        assert!(spec.calico.bgp_enabled);
        assert!(spec.calico.api_server_enabled);
        assert_eq!(spec.calico.ip_pools.len(), 1);
        assert_eq!(spec.calico.ip_pools[0].block_size, 122);
        assert_eq!(spec.calico.ip_pools[0].encapsulation, Encapsulation::None);
        assert_eq!(
            spec.calico.node_address_autodetection_v6_cidrs,
            vec!["fd97:45c2:b3a1:179::/64".to_string()]
        );
    }

    #[test]
    fn ip_pool_defaults_apply_when_omitted() {
        let json = serde_json::json!({
            "name": "default",
            "cidr": "10.244.0.0/16"
        });

        let pool: CalicoIpPoolSpec = serde_json::from_value(json).expect("pool should deserialize");

        assert_eq!(pool.encapsulation, Encapsulation::None);
        assert!(pool.nat_outgoing);
        assert_eq!(pool.block_size, 112);
        assert_eq!(pool.node_selector, "all()");
    }

    #[test]
    fn crd_definition_has_expected_group_and_kind() {
        let crd = CniInstallation::crd();
        assert_eq!(crd.spec.group, "platform.rye.ninja");
        assert_eq!(crd.spec.names.kind, "CniInstallation");
        assert_eq!(crd.spec.scope, "Cluster");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib crd:: 2>&1 | head -50`
Expected: compile errors — `CniInstallationSpec`, `PlatformKind`, etc. do not exist yet.

- [ ] **Step 3: Implement the types above the test module in `src/crd.rs`**

```rust
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CniInstallation",
    status = "CniInstallationStatus",
    shortname = "cni"
)]
#[serde(rename_all = "camelCase")]
pub struct CniInstallationSpec {
    pub platform_kind: PlatformKind,
    pub provider: CniProvider,
    pub calico: CalicoSpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformKind {
    TalosLinux,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CniProvider {
    Calico,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CalicoSpec {
    pub chart_version: String,
    #[serde(default)]
    pub bgp_enabled: bool,
    #[serde(default)]
    pub api_server_enabled: bool,
    #[serde(default)]
    pub ip_pools: Vec<CalicoIpPoolSpec>,
    #[serde(default)]
    pub node_address_autodetection_v6_cidrs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CalicoIpPoolSpec {
    pub name: String,
    pub cidr: String,
    #[serde(default)]
    pub encapsulation: Encapsulation,
    #[serde(default = "default_nat_outgoing")]
    pub nat_outgoing: bool,
    #[serde(default = "default_block_size")]
    pub block_size: i32,
    #[serde(default = "default_node_selector")]
    pub node_selector: String,
}

fn default_nat_outgoing() -> bool {
    true
}

fn default_block_size() -> i32 {
    112
}

fn default_node_selector() -> String {
    "all()".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum Encapsulation {
    #[serde(rename = "IPIP")]
    Ipip,
    #[serde(rename = "VXLAN")]
    Vxlan,
    #[default]
    #[serde(rename = "None")]
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CniInstallationStatus {
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

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Pending,
    Installing,
    Ready,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct AppliedResourceRef {
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub namespace: String,
    pub name: String,
}
```

- [ ] **Step 4: Create `src/lib.rs` exposing the module**

```rust
pub mod crd;
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib crd:: 2>&1 | tail -30`
Expected: 3 tests pass (`spec_round_trips_through_json`, `ip_pool_defaults_apply_when_omitted`, `crd_definition_has_expected_group_and_kind`).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/crd.rs
git commit -m "feat: add CniInstallation CRD types"
```

---

## Task 2: Helm values translation

**Files:**
- Create: `src/helm.rs`
- Modify: `src/lib.rs` (add `pub mod helm;`)

**Interfaces:**
- Consumes: `crate::crd::{CalicoSpec, CalicoIpPoolSpec, Encapsulation}` (Task 1).
- Produces: `platform_controller::helm::build_values(calico: &CalicoSpec) -> serde_json::Value` — used by Task 3's `render`.

- [ ] **Step 1: Write the failing tests**

Create `src/helm.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{CalicoIpPoolSpec, CalicoSpec, Encapsulation};

    fn sample_spec() -> CalicoSpec {
        CalicoSpec {
            chart_version: "v3.29.1".to_string(),
            bgp_enabled: true,
            api_server_enabled: true,
            ip_pools: vec![CalicoIpPoolSpec {
                name: "pods-v6".to_string(),
                cidr: "fd97:45c2:b3a1:1100::/56".to_string(),
                encapsulation: Encapsulation::None,
                nat_outgoing: true,
                block_size: 122,
                node_selector: "all()".to_string(),
            }],
            node_address_autodetection_v6_cidrs: vec!["fd97:45c2:b3a1:179::/64".to_string()],
        }
    }

    #[test]
    fn translates_bools_to_enabled_disabled_strings() {
        let values = build_values(&sample_spec());

        assert_eq!(values["installation"]["calicoNetwork"]["bgp"], "Enabled");
        assert_eq!(values["apiServer"]["enabled"], true);
        assert_eq!(
            values["installation"]["calicoNetwork"]["ipPools"][0]["natOutgoing"],
            "Enabled"
        );
    }

    #[test]
    fn places_node_address_autodetection_at_calico_network_level_not_per_pool() {
        let values = build_values(&sample_spec());

        assert_eq!(
            values["installation"]["calicoNetwork"]["nodeAddressAutodetectionV6"]["cidrs"][0],
            "fd97:45c2:b3a1:179::/64"
        );
        assert!(values["installation"]["calicoNetwork"]["ipPools"][0]
            .get("nodeAddressAutodetectionV6")
            .is_none());
    }

    #[test]
    fn maps_encapsulation_variants_to_chart_strings() {
        let mut spec = sample_spec();
        spec.ip_pools[0].encapsulation = Encapsulation::Vxlan;
        let values = build_values(&spec);
        assert_eq!(
            values["installation"]["calicoNetwork"]["ipPools"][0]["encapsulation"],
            "VXLAN"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib helm:: 2>&1 | head -30`
Expected: compile error — `build_values` not defined.

- [ ] **Step 3: Implement `build_values` above the test module**

```rust
use crate::crd::{CalicoSpec, Encapsulation};

pub fn build_values(calico: &CalicoSpec) -> serde_json::Value {
    let ip_pools: Vec<serde_json::Value> = calico
        .ip_pools
        .iter()
        .map(|pool| {
            serde_json::json!({
                "cidr": pool.cidr,
                "encapsulation": encapsulation_str(&pool.encapsulation),
                "natOutgoing": bool_to_enum(pool.nat_outgoing),
                "blockSize": pool.block_size,
                "nodeSelector": pool.node_selector,
            })
        })
        .collect();

    serde_json::json!({
        "installation": {
            "enabled": true,
            "calicoNetwork": {
                "bgp": bool_to_enum(calico.bgp_enabled),
                "ipPools": ip_pools,
                "nodeAddressAutodetectionV6": {
                    "cidrs": calico.node_address_autodetection_v6_cidrs,
                },
            },
        },
        "apiServer": {
            "enabled": calico.api_server_enabled,
        },
    })
}

fn bool_to_enum(value: bool) -> &'static str {
    if value {
        "Enabled"
    } else {
        "Disabled"
    }
}

fn encapsulation_str(encapsulation: &Encapsulation) -> &'static str {
    match encapsulation {
        Encapsulation::Ipip => "IPIP",
        Encapsulation::Vxlan => "VXLAN",
        Encapsulation::None => "None",
    }
}
```

- [ ] **Step 4: Wire the module into the library**

In `src/lib.rs`:

```rust
pub mod crd;
pub mod helm;
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib helm:: 2>&1 | tail -20`
Expected: 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/helm.rs
git commit -m "feat: translate CalicoSpec into tigera-operator Helm values"
```

---

## Task 3: Helm render invocation

**Files:**
- Modify: `src/helm.rs`
- Modify: `Cargo.toml` (add `serde_yaml`, `tempfile` dependencies)

**Interfaces:**
- Consumes: `build_values` (Task 2), `crate::crd::CalicoSpec`.
- Produces: `platform_controller::helm::{render, build_render_args, HelmError}` — `render(calico: &CalicoSpec) -> Result<String, HelmError>` returns the rendered multi-document YAML string, consumed by Task 4's `parse_manifests`.

- [ ] **Step 1: Add dependencies**

Run: `cargo add serde_yaml tempfile`

- [ ] **Step 2: Write the failing test**

Add to the `tests` module in `src/helm.rs`:

```rust
    #[test]
    fn render_args_pin_chart_repo_and_version() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = build_render_args("v3.29.1", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "calico".to_string(),
                "--repo".to_string(),
                "https://projectcalico.docs.tigera.io/charts".to_string(),
                "tigera-operator".to_string(),
                "--version".to_string(),
                "v3.29.1".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
            ]
        );
    }
```

Also add this opt-in integration-style test (requires the real `helm` binary and network access, so it's marked `#[ignore]`):

```rust
    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn render_produces_deployment_manifest_for_tigera_operator() {
        let spec = crate::crd::CalicoSpec {
            chart_version: "v3.29.1".to_string(),
            bgp_enabled: false,
            api_server_enabled: false,
            ip_pools: vec![],
            node_address_autodetection_v6_cidrs: vec![],
        };

        let rendered = render(&spec).await.expect("helm template should succeed");

        assert!(rendered.contains("kind: Deployment"));
        assert!(rendered.contains("tigera-operator"));
    }
```

- [ ] **Step 3: Run to verify the unignored test fails**

Run: `cargo test --lib helm::tests::render_args_pin_chart_repo_and_version 2>&1 | head -30`
Expected: compile error — `build_render_args` not defined.

- [ ] **Step 4: Implement `build_render_args`, `render`, and `HelmError`**

Add above the test module in `src/helm.rs`:

```rust
#[derive(thiserror::Error, Debug)]
pub enum HelmError {
    #[error("failed to write helm values file: {0}")]
    WriteValues(#[source] std::io::Error),
    #[error("failed to launch helm: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("helm exited with status {status}: {stderr}")]
    NonZeroExit {
        status: std::process::ExitStatus,
        stderr: String,
    },
}

pub fn build_render_args(chart_version: &str, values_path: &std::path::Path) -> Vec<String> {
    vec![
        "template".to_string(),
        "calico".to_string(),
        "--repo".to_string(),
        "https://projectcalico.docs.tigera.io/charts".to_string(),
        "tigera-operator".to_string(),
        "--version".to_string(),
        chart_version.to_string(),
        "--values".to_string(),
        values_path.display().to_string(),
        "--include-crds".to_string(),
    ]
}

pub async fn render(calico: &CalicoSpec) -> Result<String, HelmError> {
    let values = build_values(calico);
    let yaml = serde_yaml::to_string(&values).expect("serde_json::Value always serializes to YAML");

    let mut file = tempfile::NamedTempFile::new().map_err(HelmError::WriteValues)?;
    {
        use std::io::Write;
        file.write_all(yaml.as_bytes()).map_err(HelmError::WriteValues)?;
    }

    let args = build_render_args(&calico.chart_version, file.path());
    let output = tokio::process::Command::new("helm")
        .args(&args)
        .output()
        .await
        .map_err(HelmError::Spawn)?;

    if !output.status.success() {
        return Err(HelmError::NonZeroExit {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib helm:: 2>&1 | tail -20`
Expected: 4 tests pass (the 3 from Task 2 plus `render_args_pin_chart_repo_and_version`); `render_produces_deployment_manifest_for_tigera_operator` shows as `ignored`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/helm.rs
git commit -m "feat: render tigera-operator chart via helm template"
```

---

## Task 4: Manifest parsing and apply ordering

**Files:**
- Create: `src/manifests.rs`
- Modify: `src/lib.rs` (add `pub mod manifests;`)

**Interfaces:**
- Consumes: rendered YAML string from `helm::render` (Task 3).
- Produces: `platform_controller::manifests::{parse_manifests, apply_rank, sort_manifests, ManifestError}` — `parse_manifests(rendered: &str) -> Result<Vec<kube::api::DynamicObject>, ManifestError>`, `sort_manifests(objects: &mut Vec<DynamicObject>)`. Used by Task 6 (`apply_object`/`resource_ref`) and Task 7 (`reconcile`), and reused directly by Task 9's bootstrap-manifest test.

- [ ] **Step 1: Write the failing tests**

Create `src/manifests.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFESTS: &str = r#"
apiVersion: v1
kind: Namespace
metadata:
  name: tigera-operator
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: tigera-operator
  namespace: tigera-operator
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: installations.operator.tigera.io
---
apiVersion: operator.tigera.io/v1
kind: Installation
metadata:
  name: default
"#;

    #[test]
    fn parses_every_document_into_a_dynamic_object() {
        let objects = parse_manifests(SAMPLE_MANIFESTS).expect("manifests should parse");
        assert_eq!(objects.len(), 4);
        assert_eq!(objects[0].types.as_ref().unwrap().kind, "Namespace");
    }

    #[test]
    fn skips_empty_documents() {
        let objects = parse_manifests(
            "---\napiVersion: v1\nkind: Namespace\nmetadata:\n  name: x\n---\n---\n",
        )
        .expect("manifests should parse");
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn sorts_namespaces_and_crds_before_workloads_and_operator_crs_last() {
        let mut objects = parse_manifests(SAMPLE_MANIFESTS).expect("manifests should parse");
        sort_manifests(&mut objects);

        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().unwrap().kind.as_str())
            .collect();

        assert_eq!(
            kinds,
            vec!["Namespace", "CustomResourceDefinition", "Deployment", "Installation"]
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib manifests:: 2>&1 | head -30`
Expected: compile error — `parse_manifests`/`sort_manifests` not defined.

- [ ] **Step 3: Implement above the test module**

```rust
use kube::api::DynamicObject;
use serde::Deserialize;

#[derive(thiserror::Error, Debug)]
pub enum ManifestError {
    #[error("failed to parse manifest document {index} as YAML: {source}")]
    Yaml {
        index: usize,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("failed to convert manifest document {index} into a Kubernetes object: {source}")]
    Json {
        index: usize,
        #[source]
        source: serde_json::Error,
    },
}

pub fn parse_manifests(rendered: &str) -> Result<Vec<DynamicObject>, ManifestError> {
    let mut objects = Vec::new();

    for (index, document) in serde_yaml::Deserializer::from_str(rendered).enumerate() {
        let value = serde_yaml::Value::deserialize(document)
            .map_err(|source| ManifestError::Yaml { index, source })?;

        if value.is_null() {
            continue;
        }

        let json = serde_json::to_value(&value).map_err(|source| ManifestError::Json { index, source })?;
        let object: DynamicObject =
            serde_json::from_value(json).map_err(|source| ManifestError::Json { index, source })?;

        objects.push(object);
    }

    Ok(objects)
}

pub fn apply_rank(obj: &DynamicObject) -> u8 {
    let kind = obj.types.as_ref().map(|t| t.kind.as_str()).unwrap_or("");
    match kind {
        "Namespace" => 0,
        "CustomResourceDefinition" => 1,
        "ServiceAccount" | "ClusterRole" | "ClusterRoleBinding" | "Role" | "RoleBinding" => 2,
        "ConfigMap" | "Secret" | "Service" | "ValidatingWebhookConfiguration" | "APIService" => 3,
        "Deployment" | "DaemonSet" => 4,
        _ => 5,
    }
}

pub fn sort_manifests(objects: &mut Vec<DynamicObject>) {
    objects.sort_by_key(apply_rank);
}
```

- [ ] **Step 4: Wire the module into the library**

In `src/lib.rs`:

```rust
pub mod crd;
pub mod helm;
pub mod manifests;
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib manifests:: 2>&1 | tail -20`
Expected: 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/manifests.rs
git commit -m "feat: parse rendered manifests and sort them into apply order"
```

---

## Task 5: Prune diff computation

**Files:**
- Create: `src/apply.rs`
- Modify: `src/lib.rs` (add `pub mod apply;`)

**Interfaces:**
- Consumes: `crate::crd::AppliedResourceRef` (Task 1).
- Produces: `platform_controller::apply::resources_to_prune(previous: &[AppliedResourceRef], current: &[AppliedResourceRef]) -> Vec<AppliedResourceRef>` — used by Task 7's `reconcile`.

- [ ] **Step 1: Write the failing tests**

Create `src/apply.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::AppliedResourceRef;

    fn resource(kind: &str, name: &str) -> AppliedResourceRef {
        AppliedResourceRef {
            api_version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn returns_resources_present_before_but_missing_now() {
        let previous = vec![resource("ConfigMap", "a"), resource("ConfigMap", "b")];
        let current = vec![resource("ConfigMap", "b"), resource("ConfigMap", "c")];

        let pruned = resources_to_prune(&previous, &current);

        assert_eq!(pruned, vec![resource("ConfigMap", "a")]);
    }

    #[test]
    fn returns_empty_when_nothing_removed() {
        let previous = vec![resource("ConfigMap", "a")];
        let current = vec![resource("ConfigMap", "a"), resource("ConfigMap", "b")];

        assert!(resources_to_prune(&previous, &current).is_empty());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib apply:: 2>&1 | head -30`
Expected: compile error — `resources_to_prune` not defined.

- [ ] **Step 3: Implement above the test module**

```rust
use crate::crd::AppliedResourceRef;

pub fn resources_to_prune(
    previous: &[AppliedResourceRef],
    current: &[AppliedResourceRef],
) -> Vec<AppliedResourceRef> {
    previous
        .iter()
        .filter(|candidate| !current.contains(candidate))
        .cloned()
        .collect()
}
```

- [ ] **Step 4: Wire the module into the library**

In `src/lib.rs`:

```rust
pub mod apply;
pub mod crd;
pub mod helm;
pub mod manifests;
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib apply:: 2>&1 | tail -20`
Expected: 2 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/apply.rs
git commit -m "feat: compute prune diff between applied-resource snapshots"
```

---

## Task 6: Server-side apply and delete against the cluster

**Files:**
- Modify: `src/apply.rs`

**Interfaces:**
- Consumes: `kube::api::DynamicObject` (from Task 4's `parse_manifests`), `crate::crd::AppliedResourceRef`, `resources_to_prune` (Task 5).
- Produces: `platform_controller::apply::{resource_ref, apply_object, delete_object, ApplyError}` — `resource_ref(obj: &DynamicObject) -> AppliedResourceRef`, `apply_object(client: &kube::Client, obj: &DynamicObject, field_manager: &str) -> Result<AppliedResourceRef, ApplyError>`, `delete_object(client: &kube::Client, reference: &AppliedResourceRef) -> Result<(), ApplyError>`. Used by Task 7's `reconcile`.

`apply_object` and `delete_object` talk to a live Kubernetes API server via `kube::discovery`, so they are exercised end-to-end by Task 11's integration test rather than a unit test here. `resource_ref` is pure and gets its own unit test. If `kube::discovery::oneshot::pinned_kind` or `kube::api::GroupVersionKind::gvk` don't match the exact names in the resolved `kube` version, check that crate's docs (`cargo doc --open -p kube`) for the current dynamic-discovery API — the shape of the fix is the same, only names may differ.

- [ ] **Step 1: Write the failing test for `resource_ref`**

Add to the `tests` module in `src/apply.rs`:

```rust
    #[test]
    fn resource_ref_captures_gvk_namespace_and_name() {
        let objects = crate::manifests::parse_manifests(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: tigera-operator\n  namespace: tigera-operator\n",
        )
        .expect("manifest should parse");

        let reference = resource_ref(&objects[0]);

        assert_eq!(reference.api_version, "apps/v1");
        assert_eq!(reference.kind, "Deployment");
        assert_eq!(reference.namespace, "tigera-operator");
        assert_eq!(reference.name, "tigera-operator");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib apply::tests::resource_ref_captures_gvk_namespace_and_name 2>&1 | head -30`
Expected: compile error — `resource_ref` not defined.

- [ ] **Step 3: Implement `resource_ref`, `apply_object`, `delete_object`, `ApplyError`**

Add above the test module in `src/apply.rs`:

```rust
use kube::api::DynamicObject;

#[derive(thiserror::Error, Debug)]
pub enum ApplyError {
    #[error("failed to discover API resource for {api_version}/{kind}: {source}")]
    Discovery {
        api_version: String,
        kind: String,
        #[source]
        source: kube::Error,
    },
    #[error("failed to apply {kind}/{name}: {source}")]
    Patch {
        kind: String,
        name: String,
        #[source]
        source: kube::Error,
    },
}

pub fn resource_ref(obj: &DynamicObject) -> AppliedResourceRef {
    let types = obj.types.clone().unwrap_or_default();
    AppliedResourceRef {
        api_version: types.api_version,
        kind: types.kind,
        namespace: obj.metadata.namespace.clone().unwrap_or_default(),
        name: obj.metadata.name.clone().unwrap_or_default(),
    }
}

fn group_version_kind(types: &kube::api::TypeMeta) -> kube::api::GroupVersionKind {
    match types.api_version.split_once('/') {
        Some((group, version)) => kube::api::GroupVersionKind::gvk(group, version, &types.kind),
        None => kube::api::GroupVersionKind::gvk("", &types.api_version, &types.kind),
    }
}

pub async fn apply_object(
    client: &kube::Client,
    obj: &DynamicObject,
    field_manager: &str,
) -> Result<AppliedResourceRef, ApplyError> {
    let types = obj.types.clone().unwrap_or_default();
    let gvk = group_version_kind(&types);

    let (api_resource, _caps) =
        kube::discovery::oneshot::pinned_kind(client, &gvk)
            .await
            .map_err(|source| ApplyError::Discovery {
                api_version: types.api_version.clone(),
                kind: types.kind.clone(),
                source,
            })?;

    let name = obj.metadata.name.clone().unwrap_or_default();
    let api: kube::Api<DynamicObject> = match obj.metadata.namespace.clone() {
        Some(namespace) => kube::Api::namespaced_with(client.clone(), &namespace, &api_resource),
        None => kube::Api::all_with(client.clone(), &api_resource),
    };

    api.patch(
        &name,
        &kube::api::PatchParams::apply(field_manager),
        &kube::api::Patch::Apply(obj),
    )
    .await
    .map_err(|source| ApplyError::Patch {
        kind: types.kind.clone(),
        name: name.clone(),
        source,
    })?;

    Ok(resource_ref(obj))
}

pub async fn delete_object(client: &kube::Client, reference: &AppliedResourceRef) -> Result<(), ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: reference.api_version.clone(),
        kind: reference.kind.clone(),
    };
    let gvk = group_version_kind(&types);

    let (api_resource, _caps) =
        kube::discovery::oneshot::pinned_kind(client, &gvk)
            .await
            .map_err(|source| ApplyError::Discovery {
                api_version: reference.api_version.clone(),
                kind: reference.kind.clone(),
                source,
            })?;

    let api: kube::Api<DynamicObject> = if reference.namespace.is_empty() {
        kube::Api::all_with(client.clone(), &api_resource)
    } else {
        kube::Api::namespaced_with(client.clone(), &reference.namespace, &api_resource)
    };

    match api.delete(&reference.name, &kube::api::DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(()),
        Err(source) => Err(ApplyError::Patch {
            kind: reference.kind.clone(),
            name: reference.name.clone(),
            source,
        }),
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib apply:: 2>&1 | tail -20`
Expected: 3 tests pass (2 from Task 5, plus `resource_ref_captures_gvk_namespace_and_name`); the crate as a whole still builds clean: `cargo build`.

- [ ] **Step 5: Commit**

```bash
git add src/apply.rs
git commit -m "feat: apply and delete dynamic objects via server-side apply"
```

---

## Task 7: Reconciler

**Files:**
- Create: `src/reconciler.rs`
- Modify: `src/lib.rs` (add `pub mod reconciler;`)
- Modify: `Cargo.toml` (add `chrono` dependency)

**Interfaces:**
- Consumes: `crate::crd::{CniInstallation, CniInstallationSpec, CniInstallationStatus, PlatformKind, CniProvider, Phase, AppliedResourceRef}` (Task 1), `crate::helm::render` (Task 3), `crate::manifests::{parse_manifests, sort_manifests}` (Task 4), `crate::apply::{apply_object, delete_object, resources_to_prune}` (Tasks 5–6).
- Produces: `platform_controller::reconciler::{Context, ReconcileError, validate, reconcile, error_policy}` — `Context { pub client: kube::Client }`, `reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<kube::runtime::controller::Action, ReconcileError>`, `error_policy(obj: Arc<CniInstallation>, err: &ReconcileError, ctx: Arc<Context>) -> kube::runtime::controller::Action`. Used by Task 8's `main.rs`.

`reconcile` itself is glue over already-unit-tested pieces (render, parse, sort, apply, prune) plus one live status-patch call, so it is verified by `cargo build` here and exercised behaviorally by Task 11's integration test. `validate` is pure and gets its own unit test.

- [ ] **Step 1: Add the chrono dependency**

Run: `cargo add chrono --features clock`

- [ ] **Step 2: Write the failing test for `validate`**

Create `src/reconciler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{CalicoSpec, CniInstallationSpec, CniProvider, PlatformKind};

    fn spec_with(platform_kind: PlatformKind, provider: CniProvider) -> CniInstallationSpec {
        CniInstallationSpec {
            platform_kind,
            provider,
            calico: CalicoSpec {
                chart_version: "v3.29.1".to_string(),
                bgp_enabled: false,
                api_server_enabled: false,
                ip_pools: vec![],
                node_address_autodetection_v6_cidrs: vec![],
            },
        }
    }

    #[test]
    fn accepts_talos_linux_calico() {
        let spec = spec_with(PlatformKind::TalosLinux, CniProvider::Calico);
        assert!(validate(&spec).is_ok());
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib reconciler:: 2>&1 | head -30`
Expected: compile error — `validate` not defined.

- [ ] **Step 4: Implement `validate`, `Context`, `ReconcileError`, `reconcile`, `error_policy`, `update_status`**

Add above the test module in `src/reconciler.rs`:

```rust
use crate::crd::{AppliedResourceRef, CniInstallation, CniInstallationSpec, CniInstallationStatus, CniProvider, Phase, PlatformKind};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::runtime::controller::Action;
use kube::{Client, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

pub struct Context {
    pub client: Client,
}

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error("unsupported provider {0:?}, only Calico is supported")]
    UnsupportedProvider(CniProvider),
}

pub fn validate(spec: &CniInstallationSpec) -> Result<(), ValidationError> {
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    if spec.provider != CniProvider::Calico {
        return Err(ValidationError::UnsupportedProvider(spec.provider.clone()));
    }
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
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
}

pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let api: kube::Api<CniInstallation> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.calico.chart_version.clone();

    if let Err(err) = validate(&obj.spec) {
        update_status(
            &api,
            &name,
            Phase::Failed,
            obj.metadata.generation,
            &chart_version,
            &[],
            "Unsupported",
            &err.to_string(),
        )
        .await?;
        return Err(ReconcileError::Validation(err));
    }

    let rendered = crate::helm::render(&obj.spec.calico).await?;
    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);

    let mut applied = Vec::new();
    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        applied.push(reference);
    }

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();
    for stale in crate::apply::resources_to_prune(&previous, &applied) {
        crate::apply::delete_object(&ctx.client, &stale).await?;
    }

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

pub fn error_policy(_obj: Arc<CniInstallation>, _err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<CniInstallation>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), ReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()),
        observed_generation: generation,
    };

    let status = CniInstallationStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(ReconcileError::Status)?;

    Ok(())
}
```

- [ ] **Step 5: Wire the module into the library**

In `src/lib.rs`:

```rust
pub mod apply;
pub mod crd;
pub mod helm;
pub mod manifests;
pub mod reconciler;
```

- [ ] **Step 6: Run to verify it passes**

Run: `cargo test --lib reconciler:: 2>&1 | tail -20`
Expected: `accepts_talos_linux_calico` passes; `cargo build` succeeds for the whole crate.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/reconciler.rs
git commit -m "feat: reconcile CniInstallation by rendering, applying, and pruning Calico manifests"
```

---

## Task 8: Wire the controller into `main.rs`

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `platform_controller::crd::CniInstallation`, `platform_controller::reconciler::{Context, reconcile, error_policy}` (Task 7).

- [ ] **Step 1: Replace `src/main.rs`**

```rust
use futures::StreamExt;
use kube::runtime::{watcher, Controller};
use kube::{Api, Client};
use platform_controller::crd::CniInstallation;
use platform_controller::reconciler::{error_policy, reconcile, Context};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = Client::try_default().await?;
    tracing::info!(
        default_namespace = client.default_namespace(),
        "connected to kubernetes"
    );

    let api: Api<CniInstallation> = Api::all(client.clone());
    let context = Arc::new(Context { client });

    Controller::new(api, watcher::Config::default())
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        })
        .await;

    Ok(())
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build 2>&1 | tail -30`
Expected: builds with no errors.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test 2>&1 | tail -40`
Expected: all non-ignored tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: run the CniInstallation controller loop"
```

---

## Task 9: CRD generator binary and bootstrap manifest

**Files:**
- Create: `src/bin/crdgen.rs`
- Create: `deploy/bootstrap.yaml`
- Create: `deploy/crd.yaml` (generated output, checked in)
- Create: `tests/bootstrap_manifests.rs`

**Interfaces:**
- Consumes: `platform_controller::crd::CniInstallation` (Task 1), `platform_controller::manifests::{parse_manifests, sort_manifests}` (Task 4).

- [ ] **Step 1: Create the CRD generator binary**

`src/bin/crdgen.rs`:

```rust
use kube::CustomResourceExt;

fn main() {
    let crd = platform_controller::crd::CniInstallation::crd();
    print!("{}", serde_yaml::to_string(&crd).expect("CRD should serialize to YAML"));
}
```

- [ ] **Step 2: Generate the CRD manifest**

Run: `cargo run --bin crdgen > deploy/crd.yaml`

- [ ] **Step 3: Write `deploy/bootstrap.yaml`**

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: platform-system
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: platform-controller
  namespace: platform-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: platform-controller
rules:
  - apiGroups: [""]
    resources: ["namespaces", "serviceaccounts", "configmaps", "secrets", "services"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["apps"]
    resources: ["deployments", "daemonsets"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources: ["clusterroles", "clusterrolebindings", "roles", "rolebindings"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["apiextensions.k8s.io"]
    resources: ["customresourcedefinitions"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["admissionregistration.k8s.io"]
    resources: ["validatingwebhookconfigurations"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["apiregistration.k8s.io"]
    resources: ["apiservices"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["operator.tigera.io"]
    resources: ["*"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["platform.rye.ninja"]
    resources: ["cniinstallations", "cniinstallations/status"]
    verbs: ["get", "list", "watch", "update", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: platform-controller
subjects:
  - kind: ServiceAccount
    name: platform-controller
    namespace: platform-system
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: platform-controller
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: platform-controller
  namespace: platform-system
spec:
  replicas: 1
  selector:
    matchLabels:
      app: platform-controller
  template:
    metadata:
      labels:
        app: platform-controller
    spec:
      serviceAccountName: platform-controller
      hostNetwork: true
      dnsPolicy: ClusterFirstWithHostNet
      tolerations:
        - key: node.kubernetes.io/not-ready
          operator: Exists
          effect: NoSchedule
        - key: node.kubernetes.io/not-ready
          operator: Exists
          effect: NoExecute
        - key: node-role.kubernetes.io/control-plane
          operator: Exists
          effect: NoSchedule
      nodeSelector:
        node-role.kubernetes.io/control-plane: ""
      containers:
        - name: platform-controller
          image: platform-controller:latest
          env:
            - name: RUST_LOG
              value: info
---
apiVersion: platform.rye.ninja/v1alpha1
kind: CniInstallation
metadata:
  name: default
spec:
  platformKind: talos-linux
  provider: calico
  calico:
    chartVersion: v3.29.1
    bgpEnabled: false
    apiServerEnabled: false
    ipPools:
      - name: default
        cidr: 10.244.0.0/16
        encapsulation: VXLAN
        natOutgoing: true
```

- [ ] **Step 4: Write the failing test**

Create `tests/bootstrap_manifests.rs`:

```rust
use platform_controller::manifests::{parse_manifests, sort_manifests};

#[test]
fn bootstrap_yaml_parses_into_expected_kinds_in_apply_order() {
    let content = std::fs::read_to_string("deploy/bootstrap.yaml").expect("bootstrap.yaml should exist");
    let mut objects = parse_manifests(&content).expect("bootstrap.yaml should be valid YAML documents");
    sort_manifests(&mut objects);

    let kinds: Vec<String> = objects
        .iter()
        .map(|o| o.types.as_ref().unwrap().kind.clone())
        .collect();

    assert_eq!(
        kinds,
        vec![
            "Namespace",
            "ServiceAccount",
            "ClusterRole",
            "ClusterRoleBinding",
            "Deployment",
            "CniInstallation",
        ]
    );
}

#[test]
fn crd_yaml_defines_the_cniinstallation_resource() {
    let content = std::fs::read_to_string("deploy/crd.yaml")
        .expect("deploy/crd.yaml should exist; run `cargo run --bin crdgen > deploy/crd.yaml`");
    let objects = parse_manifests(&content).expect("crd.yaml should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CustomResourceDefinition");
    assert_eq!(
        objects[0].metadata.name.as_deref(),
        Some("cniinstallations.platform.rye.ninja")
    );
}
```

This test is written after `deploy/bootstrap.yaml` and `deploy/crd.yaml` already exist (Steps 2–3), so run it once to confirm it passes rather than to first watch it fail — the files not existing yet would have been the "fails" state, already covered by Steps 2–3 creating them.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -30`
Expected: both tests pass. If the CRD name assertion fails, check `deploy/crd.yaml`'s generated `metadata.name` and correct the assertion to match — `kube-derive`'s exact pluralization is authoritative, not this plan.

- [ ] **Step 6: Commit**

```bash
git add src/bin/crdgen.rs deploy/bootstrap.yaml deploy/crd.yaml tests/bootstrap_manifests.rs
git commit -m "feat: add CRD generator and cluster bootstrap manifest"
```

---

## Task 10: Container image

**Files:**
- Create: `Dockerfile`
- Create: `.dockerignore`

- [ ] **Step 1: Write `.dockerignore`**

```
target
.git
docs
```

- [ ] **Step 2: Write `Dockerfile`**

```dockerfile
FROM rust:1.82-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin platform-controller

FROM debian:bookworm-slim
ARG HELM_VERSION=v3.16.3
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-amd64.tar.gz" -o /tmp/helm.tar.gz \
    && tar -xzf /tmp/helm.tar.gz -C /tmp \
    && mv /tmp/linux-amd64/helm /usr/local/bin/helm \
    && rm -rf /tmp/helm.tar.gz /tmp/linux-amd64 \
    && apt-get purge -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/platform-controller /usr/local/bin/platform-controller
ENTRYPOINT ["/usr/local/bin/platform-controller"]
```

- [ ] **Step 3: Build the image**

Run: `docker build -t platform-controller:latest .`
Expected: build succeeds.

- [ ] **Step 4: Verify helm is bundled and runnable**

Run: `docker run --rm --entrypoint helm platform-controller:latest version`
Expected: prints a helm version string (e.g. `version.BuildInfo{Version:"v3.16.3", ...}`).

- [ ] **Step 5: Commit**

```bash
git add Dockerfile .dockerignore
git commit -m "build: package controller with bundled helm CLI"
```

---

## Task 11: Talos integration test

**Files:**
- Create: `tests/integration_talos.rs`

**Interfaces:**
- Consumes: `platform_controller::crd::{CniInstallation, Phase}` (Task 1), the full reconcile loop wired in Tasks 1–8, and `deploy/crd.yaml`/`deploy/bootstrap.yaml` (Task 9) applied to a real cluster.

This is the end-to-end proof that the MVP works, run manually (or as a dedicated, non-default CI job) rather than on every `cargo test`, since it boots real Talos nodes.

- [ ] **Step 1: Write the integration test**

Create `tests/integration_talos.rs`:

```rust
// Run manually against a real Talos-in-Docker cluster:
//   talosctl cluster create --name platform-controller-mvp --cni=none --wait
//   export KUBECONFIG=~/.talos/clusters/platform-controller-mvp/kubeconfig
//   kubectl apply -f deploy/crd.yaml
//   kubectl apply -f deploy/bootstrap.yaml
//   cargo test --test integration_talos -- --ignored --nocapture
//   talosctl cluster destroy --name platform-controller-mvp

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::Client;
use platform_controller::crd::{CniInstallation, Phase};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires a real Talos cluster with no CNI; see module docs for setup"]
async fn calico_becomes_ready_and_nodes_go_ready() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the Talos test cluster");

    let installations: Api<CniInstallation> = Api::all(client.clone());
    let nodes: Api<Node> = Api::all(client.clone());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let installation = installations
            .get("default")
            .await
            .expect("default CniInstallation should exist");

        let phase = installation
            .status
            .as_ref()
            .map(|status| status.phase.clone())
            .unwrap_or_default();
        if matches!(phase, Phase::Ready) {
            break;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "CniInstallation did not reach Ready within 5 minutes"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let node_list = nodes.list(&ListParams::default()).await.expect("should list nodes");
    for node in node_list.items {
        let ready = node
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .into_iter()
            .flatten()
            .any(|condition| condition.type_ == "Ready" && condition.status == "True");
        assert!(ready, "node {:?} did not become Ready", node.metadata.name);
    }
}
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo test --test integration_talos --no-run 2>&1 | tail -20`
Expected: compiles cleanly; the test itself is skipped by default (`ignored`).

- [ ] **Step 3: Run it for real against a Talos-in-Docker cluster**

Follow the setup comment at the top of the file, then run:
`cargo test --test integration_talos -- --ignored --nocapture`
Expected: passes — `status.phase` reaches `Ready` and every node reports `Ready=True`. Tear down the cluster afterward with `talosctl cluster destroy --name platform-controller-mvp`.

- [ ] **Step 4: Commit**

```bash
git add tests/integration_talos.rs
git commit -m "test: add end-to-end Talos integration test for Calico install"
```

---

## Self-Review Notes

- **Spec coverage:** CRD shape (§1) → Task 1. Reconcile flow steps 1–8 (§2) → Tasks 2–7 (validate/render/parse/apply/prune/status all covered), Task 8 (wiring). Bootstrap (§3) → Task 9 (manifests, RBAC), Task 10 (image). Observability (§4) → covered by `tracing` calls already in `main.rs` and status conditions in Task 7; no dedicated task needed since it's woven through existing steps rather than a separate deliverable. Fast-follows (§5) → intentionally not implemented; not a plan gap. Testing strategy (§6) → unit tests throughout Tasks 1–7, Task 11 for the integration test.
- **Placeholder scan:** no TBD/TODO; every step has runnable code and concrete run/verify commands.
- **Type consistency:** `CalicoSpec`, `AppliedResourceRef`, `Phase`, `PlatformKind`, `CniProvider` field/variant names checked consistent across Tasks 1, 2, 5, 6, 7. `resources_to_prune`, `resource_ref`, `apply_object`, `delete_object`, `parse_manifests`, `sort_manifests`, `build_values`, `build_render_args`, `render`, `validate`, `reconcile`, `error_policy` signatures checked consistent between the task that defines them and every task that calls them.
