# Pull-Through Image Cache (Spegel) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a second platform component to the controller: a `PullThroughCache` custom resource that installs Spegel (a node-to-node peer-to-peer registry mirror) on Talos clusters.

**Architecture:** A new cluster-scoped singleton CRD, `PullThroughCache`, with its own reconciler (`src/cache_reconciler.rs`) that renders the Spegel Helm chart, server-side-applies it, prunes and cleans up through the same primitives the CNI reconciler uses. `helm::render` is generalized to support OCI charts. `main.rs` runs a second `Controller` next to the CNI one, under the same leader lease. The Talos node-side containerd change stays a documented prerequisite, not something the controller applies.

**Tech Stack:** Rust (edition 2024), `kube` 4.2 / `k8s-openapi` 0.28, `schemars` 1.2, the `helm` CLI (already a runtime dependency), Spegel chart `0.7.4` from `oci://ghcr.io/spegel-org/helm-charts/spegel`.

**Spec:** [docs/superpowers/specs/2026-09-25-pull-through-cache-design.md](../specs/2026-09-25-pull-through-cache-design.md)

## Global Constraints

Every task's requirements implicitly include this section. Values are copied from the spec, plus facts confirmed against the real chart while writing this plan.

- CRD: group `platform.rye.ninja`, version `v1alpha1`, kind `PullThroughCache`, **cluster-scoped**, shortname `ptc`, singleton named `default`.
- `platformKind` accepts only `talos-linux`; `provider` accepts only `spegel`. There is no provider check in code (single-variant enum; serde rejects anything else).
- `registries` maps to the chart value `spegel.mirroredRegistries`. **Omitted keeps the chart default `[]`, which mirrors every registry.** An explicitly empty list is rejected.
- `helmValues` is merged first; the typed fields and `spegel.containerdRegistryConfigPath = /etc/cri/conf.d/hosts` are overlaid afterwards and always win.
- Namespace `spegel`, labelled `pod-security.kubernetes.io/{enforce,audit,warn}: privileged`.
- Chart: `oci://ghcr.io/spegel-org/helm-charts/spegel`. **The chart version has no `v` prefix** (`0.7.4`; the GitHub release tag `v0.7.4` does not exist as a chart tag). Render with `--include-crds --no-hooks --namespace spegel`.
- For OCI charts `helm template` prints `Pulled:` and `Digest:` lines to **stdout** before the manifests. They must be stripped before parsing.
- No `cleanupTimeoutSeconds` on this CRD.
- The finalizer name `platform.rye.ninja/cleanup` is reused (finalizers are per object).
- Existing CNI tests must pass unchanged. The CNI reconcile/cleanup logic is not modified; the only edits to `src/reconciler.rs` are making `leader_gate` and `wait_for_object_kind` `pub`.
- `Ready` means "manifests applied", not "Spegel healthy on every node" (same meaning as for `CniInstallation`).
- CI runs `cargo build`, `cargo test`, `cargo clippy --all-targets`. Clippy warnings are not denied, and `result_large_err` already fires on the existing reconciler; the new reconciler triggers it the same way and that is accepted.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Building needs free disk (a debug build of this crate is several GB). If a build fails with `No space left on device`, free space first; do not delete anything that is not a build artifact.

## Branch

Create the implementation branch from the branch that holds the spec and this plan, not from `main`:

```bash
git checkout pull-through-cache-spec
git checkout -b pull-through-cache
```

## Verification status of the code in this plan

All code in Tasks 1-6 and 8 was extracted from this plan into a scratch copy of the repo and run: `cargo build` succeeded; the full `cargo test` passed (122 unit tests; `tests/bootstrap_manifests.rs` 4, `tests/ipv6_example.rs` 6, `tests/pull_through_cache_example.rs` 5; the integration test compiles and is ignored); the real-chart test `spegel_chart_renders_parseable_manifests_with_our_values` passed against `ghcr.io`; and `cargo clippy --all-targets` reported only warning classes the existing code already triggers (`result_large_err`, `too_many_arguments`, and two pre-existing ones in `apply.rs`/`manifests.rs`).

What has **not** been run: the manual live-cluster steps (Task 8 Step 4) and the runbook's peer-serving check. The runbook's exact log line / metric name is deliberately left to be recorded from that run. If any code block here fails to compile when you apply it, treat that as a plan bug to fix, not to work around.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/helm.rs` (modify) | Generalize chart rendering: `ChartRef`/`ChartSource`, `render_args`, `render_chart`, OCI preamble stripping. CNI wrappers keep their signatures. |
| `src/pull_through_cache.rs` (create) | `PullThroughCache` CRD types, status type, spec validation, the Spegel values builder. |
| `src/crds.rs` (create) | `generated_yaml()`: every CRD as the multi-document `deploy/crd.yaml`. |
| `src/cache_reconciler.rs` (create) | Reconcile, cleanup, finalizer wiring, status for `PullThroughCache`. |
| `src/reconciler.rs` (modify, 2 words) | `leader_gate` and `wait_for_object_kind` become `pub`. |
| `src/main.rs` (modify) | Generic `deletion_requested`; second watcher + `Controller<PullThroughCache>`. |
| `src/bin/crdgen.rs`, `src/lib.rs` (modify) | Emit both CRDs; register new modules. |
| `deploy/crd.yaml` (regenerate), `deploy/README.md` (modify) | Both CRDs; apply/wait sequence. |
| `examples/pull-through-cache.yaml` (create) | Talos starting point. |
| `tests/bootstrap_manifests.rs` (modify) | CRD-file tests for two CRDs. |
| `tests/pull_through_cache_example.rs` (create) | Example manifest is valid. |
| `tests/integration_pull_through_cache.rs` (create) | Ignored live-cluster test. |
| `docs/runbooks/pull-through-cache-verification.md`, `docs/memory/*` (create/modify), spec (modify) | Prerequisite, live verification, memory, spec corrections. |

---

## Task 1: Generalize Helm rendering for OCI charts

**Files:**
- Modify: `src/helm.rs` (constants after `CALICO_CHART_REPO`; replace `build_render_args` and `render`; add tests)

**Interfaces:**
- Consumes: existing `CalicoSpec`, `build_values`, `HelmError`, `TIGERA_OPERATOR_NAMESPACE`, `CALICO_CHART_REPO`.
- Produces (all `pub` in `crate::helm`):
  - `const SPEGEL_NAMESPACE: &str = "spegel"`
  - `enum ChartSource { Repo { url: &'static str, chart: &'static str }, Oci { reference: &'static str } }`
  - `struct ChartRef { release: &'static str, source: ChartSource, namespace: &'static str }`
  - `const CALICO_CHART: ChartRef`, `const SPEGEL_CHART: ChartRef`
  - `fn render_args(chart: &ChartRef, chart_version: &str, values_path: &std::path::Path) -> Vec<String>`
  - `async fn render_chart(chart: &ChartRef, chart_version: &str, values: &serde_json::Value) -> Result<String, HelmError>`
  - Unchanged signatures: `build_render_args(&str, &Path) -> Vec<String>`, `render(&CalicoSpec) -> Result<String, HelmError>`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/helm.rs`, immediately before `omits_node_address_autodetection_v6_when_no_cidrs_configured`:

```rust
    #[test]
    fn oci_render_args_pass_the_reference_without_a_repo_flag() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&SPEGEL_CHART, "v0.0.0-test", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "spegel".to_string(),
                "oci://ghcr.io/spegel-org/helm-charts/spegel".to_string(),
                "--version".to_string(),
                "v0.0.0-test".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "spegel".to_string(),
            ]
        );
    }

    #[test]
    fn strips_the_oci_pull_progress_lines_before_the_first_manifest() {
        let output = "Pulled: ghcr.io/spegel-org/helm-charts/spegel:0.7.4\n\
                      Digest: sha256:abc\n\
                      ---\n\
                      # Source: spegel/templates/rbac.yaml\n\
                      kind: ServiceAccount\n";

        assert_eq!(
            strip_oci_pull_preamble(output),
            "---\n# Source: spegel/templates/rbac.yaml\nkind: ServiceAccount\n"
        );
    }

    #[test]
    fn leaves_repo_chart_output_untouched() {
        let output = "---\n# Source: tigera-operator/templates/x.yaml\nkind: Deployment\n";

        assert_eq!(strip_oci_pull_preamble(output), output);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib helm::tests`
Expected: compile error, `cannot find function \`render_args\`` / `cannot find value \`SPEGEL_CHART\`` / `cannot find function \`strip_oci_pull_preamble\``.

- [ ] **Step 3: Add the constants and types**

In `src/helm.rs`, directly after the `CALICO_CHART_REPO` constant, add:

```rust
/// Namespace the Spegel chart's namespaced objects belong in. Like
/// tigera-operator, the chart renders no `Namespace` object of its own.
pub const SPEGEL_NAMESPACE: &str = "spegel";

/// Where a Helm chart is fetched from. The two forms invoke `helm template`
/// differently.
pub enum ChartSource {
    /// `helm template <release> --repo <url> <chart>`
    Repo { url: &'static str, chart: &'static str },
    /// `helm template <release> <reference>`, where the reference is `oci://...`
    Oci { reference: &'static str },
}

/// Everything about a chart except its version and values.
pub struct ChartRef {
    pub release: &'static str,
    pub source: ChartSource,
    pub namespace: &'static str,
}

pub const CALICO_CHART: ChartRef = ChartRef {
    release: "calico",
    source: ChartSource::Repo {
        url: CALICO_CHART_REPO,
        chart: "tigera-operator",
    },
    namespace: TIGERA_OPERATOR_NAMESPACE,
};

pub const SPEGEL_CHART: ChartRef = ChartRef {
    release: "spegel",
    source: ChartSource::Oci {
        reference: "oci://ghcr.io/spegel-org/helm-charts/spegel",
    },
    namespace: SPEGEL_NAMESPACE,
};
```

- [ ] **Step 4: Replace `build_render_args` and `render`**

Replace everything from `pub fn build_render_args` up to (not including) `#[cfg(test)]` with:

```rust
pub fn render_args(chart: &ChartRef, chart_version: &str, values_path: &std::path::Path) -> Vec<String> {
    let mut args = vec!["template".to_string(), chart.release.to_string()];
    match &chart.source {
        ChartSource::Repo { url, chart: name } => {
            args.extend(["--repo".to_string(), url.to_string(), name.to_string()]);
        }
        ChartSource::Oci { reference } => args.push(reference.to_string()),
    }
    args.extend([
        "--version".to_string(),
        chart_version.to_string(),
        "--values".to_string(),
        values_path.display().to_string(),
        "--include-crds".to_string(),
        // Without --no-hooks the chart emits its hook Jobs (e.g. the
        // tigera-operator-uninstall Job), which this controller would then
        // apply as live objects -- immediately tearing the component back down.
        "--no-hooks".to_string(),
        // Without an explicit namespace, helm resolves .Release.Namespace from
        // ambient kubeconfig context, so namespaced objects land in the wrong
        // namespace (or "default") instead of the chart's own namespace.
        "--namespace".to_string(),
        chart.namespace.to_string(),
    ]);
    args
}

pub fn build_render_args(chart_version: &str, values_path: &std::path::Path) -> Vec<String> {
    render_args(&CALICO_CHART, chart_version, values_path)
}

pub async fn render_chart(
    chart: &ChartRef,
    chart_version: &str,
    values: &serde_json::Value,
) -> Result<String, HelmError> {
    let yaml = serde_yaml::to_string(values).expect("serde_json::Value always serializes to YAML");

    let mut file = tempfile::NamedTempFile::new().map_err(HelmError::WriteValues)?;
    {
        use std::io::Write;
        file.write_all(yaml.as_bytes()).map_err(HelmError::WriteValues)?;
    }

    let args = render_args(chart, chart_version, file.path());
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

    Ok(strip_oci_pull_preamble(&String::from_utf8_lossy(&output.stdout)).to_string())
}

/// `helm template` on an `oci://` chart prints `Pulled:` and `Digest:` progress
/// lines to stdout ahead of the manifests. Left in place they parse as a
/// bogus first manifest with no `apiVersion`/`kind`, so drop them. Repo charts
/// print no such lines and pass through unchanged.
fn strip_oci_pull_preamble(output: &str) -> &str {
    let mut rest = output;
    while let Some(line_end) = rest.find('\n') {
        let line = &rest[..line_end];
        if line.starts_with("Pulled: ") || line.starts_with("Digest: ") {
            rest = &rest[line_end + 1..];
        } else {
            break;
        }
    }
    rest
}

pub async fn render(calico: &CalicoSpec) -> Result<String, HelmError> {
    render_chart(&CALICO_CHART, &calico.chart_version, &build_values(calico)).await
}

```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib helm::tests`
Expected: PASS, including the pre-existing `render_args_pin_chart_repo_and_version` (it exercises `build_render_args` unchanged) and the two `#[ignore]` network tests reported as ignored.

- [ ] **Step 6: Commit**

```bash
git add src/helm.rs
git commit -m "$(cat <<'EOF'
refactor: generalize helm rendering to support OCI charts

ChartRef/ChartSource let render_args and render_chart drive both the
Calico repo chart and OCI charts. helm prints Pulled:/Digest: lines to
stdout for OCI charts; strip them so they are not parsed as a manifest.
The CNI wrappers keep their signatures.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `PullThroughCache` types, validation and Spegel values

**Files:**
- Create: `src/pull_through_cache.rs`
- Modify: `src/lib.rs` (add `pub mod pull_through_cache;`)
- Modify: `src/helm.rs` (add one `#[ignore]`d real-chart test)

**Interfaces:**
- Consumes: `crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind}`.
- Produces (all `pub` in `crate::pull_through_cache`):
  - `const TALOS_CONTAINERD_REGISTRY_CONFIG_PATH: &str = "/etc/cri/conf.d/hosts"`
  - `struct PullThroughCacheSpec { platform_kind: PlatformKind, provider: CacheProvider, spegel: SpegelSpec }` and the generated `PullThroughCache` resource type (`PullThroughCache::crd()` via `kube::CustomResourceExt`)
  - `enum CacheProvider { Spegel }`
  - `struct SpegelSpec { chart_version: String, registries: Option<Vec<String>>, helm_values: Option<serde_json::Value> }` (implements `Default`)
  - `struct PullThroughCacheStatus { phase: Phase, observed_generation: i64, chart_version: String, applied_resources: Vec<AppliedResourceRef>, conditions: Vec<Condition> }`
  - `enum CacheSpecError { EmptyChartVersion, EmptyRegistries, InvalidRegistry(String), HelmValuesNotObject }` with `fn reason(&self) -> &'static str`
  - `fn validate_spegel(spegel: &SpegelSpec) -> Result<(), CacheSpecError>`
  - `fn build_values(spegel: &SpegelSpec) -> serde_json::Value`

- [ ] **Step 1: Register the module and create the file with types and tests**

In `src/lib.rs`, add `pub mod pull_through_cache;` (keep the list alphabetical: after `manifests`, before `reconciler`).

Create `src/pull_through_cache.rs` with the types, then the tests (the functions come in Step 3):

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where containerd on Talos reads per-registry mirror config from. Spegel
/// writes its `hosts.toml` files here, and Talos uses a different path than
/// the chart's default, so this is set unconditionally for `talos-linux`.
pub const TALOS_CONTAINERD_REGISTRY_CONFIG_PATH: &str = "/etc/cri/conf.d/hosts";

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "PullThroughCache",
    status = "PullThroughCacheStatus",
    shortname = "ptc"
)]
#[serde(rename_all = "camelCase")]
pub struct PullThroughCacheSpec {
    pub platform_kind: PlatformKind,
    pub provider: CacheProvider,
    pub spegel: SpegelSpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheProvider {
    Spegel,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SpegelSpec {
    pub chart_version: String,
    /// Upstream registries to mirror, as bare hostnames (optionally `host:port`).
    /// Omitted means the chart default, which mirrors every registry. An empty
    /// list is rejected as ambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registries: Option<Vec<String>>,
    /// Free-form values merged into the chart's values. Typed fields and the
    /// controller's own Talos setting are overlaid on top, so they always win.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "preserve_unknown_object")]
    pub helm_values: Option<serde_json::Value>,
}

fn preserve_unknown_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true
    })
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PullThroughCacheStatus {
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
pub enum CacheSpecError {
    #[error("spec.spegel.chartVersion must not be empty")]
    EmptyChartVersion,
    #[error("spec.spegel.registries is empty; omit the field to mirror every registry")]
    EmptyRegistries,
    #[error(
        "registries entry {0:?} is not a bare hostname, optionally with a numeric :port \
         (no scheme, path or IPv6 literal)"
    )]
    InvalidRegistry(String),
    #[error("spec.spegel.helmValues must be a JSON object")]
    HelmValuesNotObject,
}

impl CacheSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            CacheSpecError::EmptyChartVersion => "InvalidChartVersion",
            CacheSpecError::EmptyRegistries | CacheSpecError::InvalidRegistry(_) => {
                "InvalidRegistry"
            }
            CacheSpecError::HelmValuesNotObject => "InvalidHelmValues",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn spegel() -> SpegelSpec {
        SpegelSpec {
            chart_version: "v0.0.0-test".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn spec_deserializes_with_only_the_required_fields() {
        let spec: PullThroughCacheSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "spegel",
            "spegel": { "chartVersion": "v0.0.0-test" }
        }))
        .expect("minimal spec should deserialize");

        assert_eq!(spec.platform_kind, PlatformKind::TalosLinux);
        assert_eq!(spec.provider, CacheProvider::Spegel);
        assert_eq!(spec.spegel.chart_version, "v0.0.0-test");
        assert!(spec.spegel.registries.is_none());
        assert!(spec.spegel.helm_values.is_none());
    }

    #[test]
    fn spec_deserializes_registries_and_helm_values() {
        let spec: PullThroughCacheSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "spegel",
            "spegel": {
                "chartVersion": "v0.0.0-test",
                "registries": ["docker.io", "ghcr.io"],
                "helmValues": { "resources": { "limits": { "memory": "128Mi" } } }
            }
        }))
        .expect("full spec should deserialize");

        assert_eq!(
            spec.spegel.registries,
            Some(vec!["docker.io".to_string(), "ghcr.io".to_string()])
        );
        assert_eq!(
            spec.spegel.helm_values.unwrap()["resources"]["limits"]["memory"],
            "128Mi"
        );
    }

    #[test]
    fn unknown_providers_are_rejected_at_deserialization() {
        let result = serde_json::from_value::<PullThroughCacheSpec>(serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "aws-ecr",
            "spegel": { "chartVersion": "v0.0.0-test" }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn accepts_a_minimal_spec() {
        assert_eq!(validate_spegel(&spegel()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_chart_version() {
        let spec = SpegelSpec {
            chart_version: "  ".to_string(),
            ..spegel()
        };

        let err = validate_spegel(&spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err, CacheSpecError::EmptyChartVersion);
        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn rejects_an_explicitly_empty_registries_list() {
        let spec = SpegelSpec {
            registries: Some(vec![]),
            ..spegel()
        };

        let err = validate_spegel(&spec).expect_err("an empty list is ambiguous");

        assert_eq!(err, CacheSpecError::EmptyRegistries);
        assert_eq!(err.reason(), "InvalidRegistry");
    }

    #[test]
    fn accepts_bare_hostnames_with_optional_ports() {
        let spec = SpegelSpec {
            registries: Some(vec![
                "docker.io".to_string(),
                "registry.k8s.io".to_string(),
                "localhost:5000".to_string(),
                "my-registry.example.com:8443".to_string(),
            ]),
            ..spegel()
        };

        assert_eq!(validate_spegel(&spec), Ok(()));
    }

    #[test]
    fn rejects_registries_that_are_not_bare_hostnames() {
        for bad in [
            "https://docker.io",
            "docker.io/library",
            "docker.io:",
            "docker.io:port",
            "",
            "-docker.io",
            "docker.io.",
            "[fd00::1]:5000",
            "docker io",
        ] {
            let spec = SpegelSpec {
                registries: Some(vec!["ghcr.io".to_string(), bad.to_string()]),
                ..spegel()
            };

            let err = validate_spegel(&spec)
                .expect_err(&format!("{bad:?} should be rejected as a registry"));

            assert_eq!(err, CacheSpecError::InvalidRegistry(bad.to_string()));
            assert_eq!(err.reason(), "InvalidRegistry");
        }
    }

    #[test]
    fn rejects_helm_values_that_are_not_an_object() {
        let spec = SpegelSpec {
            helm_values: Some(serde_json::json!(["not", "an", "object"])),
            ..spegel()
        };

        let err = validate_spegel(&spec).expect_err("helmValues must be an object");

        assert_eq!(err, CacheSpecError::HelmValuesNotObject);
        assert_eq!(err.reason(), "InvalidHelmValues");
    }

    #[test]
    fn values_always_set_the_talos_containerd_config_path() {
        let values = build_values(&spegel());

        assert_eq!(
            values["spegel"]["containerdRegistryConfigPath"],
            "/etc/cri/conf.d/hosts"
        );
    }

    #[test]
    fn omitted_registries_leave_mirrored_registries_unset() {
        let values = build_values(&spegel());

        assert!(values["spegel"].get("mirroredRegistries").is_none());
    }

    #[test]
    fn registries_map_to_mirrored_registries() {
        let spec = SpegelSpec {
            registries: Some(vec!["docker.io".to_string(), "ghcr.io".to_string()]),
            ..spegel()
        };

        let values = build_values(&spec);

        assert_eq!(
            values["spegel"]["mirroredRegistries"],
            serde_json::json!(["docker.io", "ghcr.io"])
        );
    }

    #[test]
    fn helm_values_pass_through_alongside_the_typed_values() {
        let spec = SpegelSpec {
            helm_values: Some(serde_json::json!({
                "resources": { "limits": { "memory": "128Mi" } },
                "spegel": { "logLevel": "DEBUG" }
            })),
            ..spegel()
        };

        let values = build_values(&spec);

        assert_eq!(values["resources"]["limits"]["memory"], "128Mi");
        assert_eq!(values["spegel"]["logLevel"], "DEBUG");
        assert_eq!(
            values["spegel"]["containerdRegistryConfigPath"],
            "/etc/cri/conf.d/hosts"
        );
    }

    #[test]
    fn typed_fields_win_over_conflicting_helm_values() {
        let spec = SpegelSpec {
            registries: Some(vec!["ghcr.io".to_string()]),
            helm_values: Some(serde_json::json!({
                "spegel": {
                    "containerdRegistryConfigPath": "/somewhere/else",
                    "mirroredRegistries": ["docker.io"]
                }
            })),
            ..spegel()
        };

        let values = build_values(&spec);

        assert_eq!(
            values["spegel"]["containerdRegistryConfigPath"],
            "/etc/cri/conf.d/hosts"
        );
        assert_eq!(
            values["spegel"]["mirroredRegistries"],
            serde_json::json!(["ghcr.io"])
        );
    }

    #[test]
    fn helm_values_passthrough_survives_when_registries_are_omitted() {
        let spec = SpegelSpec {
            helm_values: Some(serde_json::json!({
                "spegel": { "mirroredRegistries": ["quay.io"] }
            })),
            ..spegel()
        };

        let values = build_values(&spec);

        assert_eq!(
            values["spegel"]["mirroredRegistries"],
            serde_json::json!(["quay.io"])
        );
    }

    #[test]
    fn crd_is_cluster_scoped_with_the_ptc_shortname() {
        let crd = PullThroughCache::crd();

        assert_eq!(
            crd.metadata.name.as_deref(),
            Some("pullthroughcaches.platform.rye.ninja")
        );
        assert_eq!(crd.spec.scope, "Cluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["ptc".to_string()]));
    }

    #[test]
    fn crd_schema_lets_helm_values_carry_arbitrary_keys() {
        let yaml = serde_yaml::to_string(&PullThroughCache::crd()).expect("CRD serializes");

        assert!(
            yaml.contains("x-kubernetes-preserve-unknown-fields: true"),
            "helmValues must be a structural, unknown-field-preserving object:\n{yaml}"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib pull_through_cache`
Expected: compile error, `cannot find function \`validate_spegel\`` and `cannot find function \`build_values\``.

- [ ] **Step 3: Add the validation and values functions**

Insert directly above `#[cfg(test)]` in `src/pull_through_cache.rs`:

```rust
/// A bare hostname with an optional numeric port: `docker.io`, `localhost:5000`.
fn is_bare_registry_host(entry: &str) -> bool {
    let host = match entry.rsplit_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            host
        }
        None => entry,
    };
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    edge_ok(host.chars().next())
        && edge_ok(host.chars().last())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Rejects specs the chart would silently mis-handle, before anything is
/// applied.
pub fn validate_spegel(spegel: &SpegelSpec) -> Result<(), CacheSpecError> {
    if spegel.chart_version.trim().is_empty() {
        return Err(CacheSpecError::EmptyChartVersion);
    }
    if let Some(registries) = &spegel.registries {
        if registries.is_empty() {
            return Err(CacheSpecError::EmptyRegistries);
        }
        if let Some(bad) = registries.iter().find(|entry| !is_bare_registry_host(entry)) {
            return Err(CacheSpecError::InvalidRegistry(bad.clone()));
        }
    }
    if let Some(values) = &spegel.helm_values
        && !values.is_object()
    {
        return Err(CacheSpecError::HelmValuesNotObject);
    }
    Ok(())
}

/// Recursively merges `overlay` into `base`; on a conflict the overlay wins.
fn merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// The Helm values for the Spegel chart: the user's `helmValues` passthrough,
/// with the typed fields and the Talos containerd path overlaid on top.
pub fn build_values(spegel: &SpegelSpec) -> serde_json::Value {
    let mut values = spegel
        .helm_values
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));

    let mut typed = serde_json::json!({
        "spegel": {
            "containerdRegistryConfigPath": TALOS_CONTAINERD_REGISTRY_CONFIG_PATH,
        },
    });
    if let Some(registries) = &spegel.registries {
        typed["spegel"]["mirroredRegistries"] = serde_json::json!(registries);
    }

    merge(&mut values, typed);
    values
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib pull_through_cache`
Expected: PASS (17 tests), including `crd_schema_lets_helm_values_carry_arbitrary_keys`, which proves the `schema_with` produces a structural schema.

- [ ] **Step 5: Add the real-chart test to `src/helm.rs`**

In the `tests` module of `src/helm.rs`, immediately before the existing `v3_32_1_chart_ships_no_crds` test, add:

```rust
    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn spegel_chart_renders_parseable_manifests_with_our_values() {
        let spegel = crate::pull_through_cache::SpegelSpec {
            chart_version: "0.7.4".to_string(),
            registries: Some(vec!["docker.io".to_string(), "ghcr.io".to_string()]),
            ..Default::default()
        };
        let values = crate::pull_through_cache::build_values(&spegel);

        let rendered = render_chart(&SPEGEL_CHART, &spegel.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        // Every document must be a real Kubernetes object; the OCI `Pulled:` lines
        // would otherwise parse as one with no apiVersion/kind.
        for object in &objects {
            let types = object.types.as_ref().expect("every rendered object has a type");
            assert!(!types.kind.is_empty(), "{object:?}");
        }
        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().unwrap().kind.as_str())
            .collect();
        assert!(kinds.contains(&"DaemonSet"), "{kinds:?}");
        assert!(rendered.contains("--containerd-registry-config-path=/etc/cri/conf.d/hosts"));
        // --no-hooks: the post-delete cleanup hook must not be rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }
```

- [ ] **Step 6: Run the real-chart test**

Run: `cargo test --lib spegel_chart_renders -- --ignored`
Expected: PASS (needs network access to `ghcr.io` and `helm` on `PATH`).

- [ ] **Step 7: Commit**

```bash
git add src/pull_through_cache.rs src/lib.rs src/helm.rs
git commit -m "$(cat <<'EOF'
feat: PullThroughCache CRD types, validation and Spegel values

Cluster-scoped singleton with a typed Spegel spec. registries maps to the
chart's mirroredRegistries; typed fields and the Talos containerd config
path always win over the helmValues passthrough. Includes an ignored test
that renders the real chart from ghcr.io.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Emit both CRDs into `deploy/crd.yaml`

**Files:**
- Create: `src/crds.rs`
- Modify: `src/lib.rs` (add `pub mod crds;`), `src/bin/crdgen.rs`, `tests/bootstrap_manifests.rs`
- Regenerate: `deploy/crd.yaml`

**Interfaces:**
- Consumes: `crate::crd::CniInstallation`, `crate::pull_through_cache::PullThroughCache`.
- Produces: `pub fn generated_yaml() -> String` in `crate::crds`: both CRDs as one multi-document YAML string (documents joined with `---\n`, CNI first).

- [ ] **Step 1: Replace the two CRD-file tests**

In `tests/bootstrap_manifests.rs`, delete `crd_yaml_defines_the_cniinstallation_resource` and `crd_yaml_matches_the_generated_crd` (everything from `#[test]\nfn crd_yaml_defines_the_cniinstallation_resource` to the end of the file) and put in their place:

```rust
#[test]
fn crd_yaml_defines_both_platform_resources() {
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
        ]
    );
}

#[test]
fn crd_yaml_matches_the_generated_crds() {
    let generated = platform_controller::crds::generated_yaml();
    let on_disk = std::fs::read_to_string("deploy/crd.yaml").expect("deploy/crd.yaml should exist");

    assert_eq!(
        on_disk, generated,
        "deploy/crd.yaml is stale; run `cargo run -q --bin crdgen > deploy/crd.yaml`"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test bootstrap_manifests`
Expected: compile error, `could not find \`crds\` in \`platform_controller\``.

- [ ] **Step 3: Create `src/crds.rs`, register it, and point `crdgen` at it**

`src/crds.rs`:

```rust
use kube::CustomResourceExt;

/// Every CRD this controller owns, as the multi-document YAML kept in
/// `deploy/crd.yaml`. Shared by `crdgen` and the test that keeps the file fresh.
pub fn generated_yaml() -> String {
    [
        crate::crd::CniInstallation::crd(),
        crate::pull_through_cache::PullThroughCache::crd(),
    ]
    .iter()
    .map(|crd| serde_yaml::to_string(crd).expect("CRD should serialize to YAML"))
    .collect::<Vec<_>>()
    .join("---\n")
}
```

In `src/lib.rs`, add `pub mod crds;` (after `pub mod crd;`).

Replace the whole of `src/bin/crdgen.rs` with:

```rust
fn main() {
    print!("{}", platform_controller::crds::generated_yaml());
}
```

- [ ] **Step 4: Regenerate `deploy/crd.yaml`**

Run: `cargo run -q --bin crdgen > deploy/crd.yaml`
Then: `grep -c "^kind: CustomResourceDefinition" deploy/crd.yaml`
Expected: `2`. Also `grep -n "^---" deploy/crd.yaml` shows exactly one separator, and `grep -B3 "x-kubernetes-preserve-unknown-fields" deploy/crd.yaml` shows it under `helmValues`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test bootstrap_manifests`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add src/crds.rs src/lib.rs src/bin/crdgen.rs tests/bootstrap_manifests.rs deploy/crd.yaml
git commit -m "$(cat <<'EOF'
feat: generate both CRDs into deploy/crd.yaml

crds::generated_yaml joins the CniInstallation and PullThroughCache CRDs;
crdgen and the freshness test share it.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: The `PullThroughCache` reconciler

**Files:**
- Create: `src/cache_reconciler.rs`
- Modify: `src/lib.rs` (add `pub mod cache_reconciler;`, first in the list)
- Modify: `src/reconciler.rs` (two visibility changes)

**Interfaces:**
- Consumes: from `crate::reconciler` (now `pub`): `leader_gate(&AtomicBool) -> Option<Action>`, `wait_for_object_kind(&Client, &DynamicObject) -> Result<(), ApplyError>`, plus existing `Context`, `FINALIZER_NAME`, `SINGLETON_NAME`; from `crate::helm`: `render_chart`, `SPEGEL_CHART`, `SPEGEL_NAMESPACE`; from `crate::pull_through_cache`: `build_values`, `validate_spegel`, the types; from `crate::apply` / `crate::manifests` (unchanged).
- Produces (`pub` in `crate::cache_reconciler`):
  - `enum ValidationError { UnsupportedPlatform(PlatformKind), UnsupportedName(String), Spec(CacheSpecError) }` with `fn reason(&self) -> &'static str`
  - `fn validate(name: &str, spec: &PullThroughCacheSpec) -> Result<(), ValidationError>`
  - `fn spegel_namespace_object() -> DynamicObject`
  - `enum CacheReconcileError { Validation, Helm, Manifest, Apply, Status(kube::Error), NotLeader }`
  - `async fn reconcile(Arc<PullThroughCache>, Arc<Context>) -> Result<Action, CacheReconcileError>`
  - `async fn cleanup(Arc<PullThroughCache>, Arc<Context>) -> Result<Action, CacheReconcileError>`
  - `async fn reconcile_with_finalizer(Arc<PullThroughCache>, Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<CacheReconcileError>>`
  - `fn error_policy(Arc<PullThroughCache>, &kube::runtime::finalizer::Error<CacheReconcileError>, Arc<Context>) -> Action`

- [ ] **Step 1: Make the two shared helpers `pub` and register the module**

In `src/reconciler.rs`, change `fn leader_gate(` to `pub fn leader_gate(` and `async fn wait_for_object_kind(` to `pub async fn wait_for_object_kind(`. In `src/lib.rs`, add `pub mod cache_reconciler;` as the first `pub mod` line (alphabetical: before `calico`).

- [ ] **Step 2: Write the failing tests**

Create `src/cache_reconciler.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_through_cache::{CacheProvider, SpegelSpec};

    fn spec_with(platform_kind: PlatformKind) -> PullThroughCacheSpec {
        PullThroughCacheSpec {
            platform_kind,
            provider: CacheProvider::Spegel,
            spegel: SpegelSpec {
                chart_version: "v0.0.0-test".to_string(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn accepts_talos_linux_spegel_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_caches_not_named_default() {
        let err = validate("second", &spec_with(PlatformKind::TalosLinux))
            .expect_err("non-singleton names should be rejected");

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.spegel.chart_version = String::new();

        let err = validate("default", &spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn synthesized_namespace_is_privileged_and_named_spegel() {
        let object = spegel_namespace_object();
        let types = object.types.as_ref().expect("types should be set");
        let labels = object.metadata.labels.as_ref().expect("labels should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("spegel"));
        assert!(object.metadata.namespace.is_none());
        for mode in ["enforce", "audit", "warn"] {
            assert_eq!(
                labels.get(&format!("pod-security.kubernetes.io/{mode}")).map(String::as_str),
                Some("privileged")
            );
        }
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&spegel_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "spegel");
        assert_eq!(reference.namespace, "");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib cache_reconciler`
Expected: compile errors, `cannot find function \`validate\``, `cannot find function \`spegel_namespace_object\``, `cannot find type \`ValidationError\``.

- [ ] **Step 4: Write the reconciler**

Insert at the top of `src/cache_reconciler.rs`, above the `#[cfg(test)]` block:

```rust
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::pull_through_cache::{
    CacheSpecError, PullThroughCache, PullThroughCacheSpec, PullThroughCacheStatus,
};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME};
use kube::api::DynamicObject;
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "PullThroughCache {0:?} is ignored; this controller only reconciles the cluster-scoped \
         singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] CacheSpecError),
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

/// There is no provider check: `CacheProvider` has one variant and serde already
/// rejects any other value. Add one alongside the second provider.
pub fn validate(name: &str, spec: &PullThroughCacheSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::pull_through_cache::validate_spegel(&spec.spegel)?;
    Ok(())
}

/// The Spegel chart renders no `Namespace` object, so the controller synthesizes
/// one and applies it ahead of everything else. It is also tracked in
/// `status.appliedResources` so prune semantics stay consistent.
///
/// Spegel mounts the containerd socket and host paths, which Talos's default
/// `baseline` Pod Security Standard rejects, so the namespace is labelled
/// `privileged`.
pub fn spegel_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::SPEGEL_NAMESPACE,
            "labels": {
                "pod-security.kubernetes.io/enforce": "privileged",
                "pod-security.kubernetes.io/audit": "privileged",
                "pod-security.kubernetes.io/warn": "privileged",
            },
        },
    }))
    .expect("static Namespace JSON deserializes into a DynamicObject")
}

#[derive(thiserror::Error, Debug)]
pub enum CacheReconcileError {
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

pub async fn reconcile(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, CacheReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<PullThroughCache> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.spegel.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(cache = %name, error = %err, "validation failed");
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
        return Err(CacheReconcileError::Validation(err));
    }
    tracing::info!(
        cache = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::pull_through_cache::build_values(&obj.spec.spegel);
    let rendered =
        crate::helm::render_chart(&crate::helm::SPEGEL_CHART, &chart_version, &values).await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::SPEGEL_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered spegel chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    let mut applied = Vec::new();

    // The chart has no Namespace object of its own; create the target namespace
    // before anything that lives inside it.
    let namespace_ref =
        crate::apply::apply_object(&ctx.client, &spegel_namespace_object(), "platform-controller")
            .await?;
    applied.push(namespace_ref);

    for object in &objects {
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

    tracing::info!(cache = %name, phase = ?Phase::Ready, "updating status");
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
    _obj: Arc<PullThroughCache>,
    _err: &kube::runtime::finalizer::Error<CacheReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    api: &kube::Api<PullThroughCache>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CacheReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = PullThroughCacheStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CacheReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, CacheReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CacheReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(cache = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order: the namespace was applied first, so it goes last.
    // Spegel owns no resources that need a bounded removal wait.
    for reference in applied.iter().rev() {
        tracing::info!(
            cache = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(cache = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CacheReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all; see
    // `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<PullThroughCache> = kube::Api::all(ctx.client.clone());
    kube::runtime::finalizer(&api, FINALIZER_NAME, obj, |event| async move {
        match event {
            kube::runtime::finalizer::Event::Apply(obj) => reconcile(obj, ctx).await,
            kube::runtime::finalizer::Event::Cleanup(obj) => cleanup(obj, ctx).await,
        }
    })
    .await
}

```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cache_reconciler && cargo test --lib reconciler`
Expected: PASS for both (the CNI reconciler tests are unaffected by the visibility change).

- [ ] **Step 6: Commit**

```bash
git add src/cache_reconciler.rs src/reconciler.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: reconcile PullThroughCache by rendering and applying Spegel

Same shape as the CNI loop: validate, render, apply with a synthesized
privileged namespace, prune against the status ledger, finalizer cleanup
in reverse ledger order. leader_gate and wait_for_object_kind are shared
by making them pub rather than duplicating them.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Run a second controller in `main.rs`

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `cache_reconciler::{reconcile_with_finalizer, error_policy}`, `pull_through_cache::PullThroughCache`, the existing `Context`, `reflector`, `watcher`, `Controller`.
- Produces: `fn deletion_requested<K: Resource>(obj: &K) -> Option<u64>` (generic; existing tests call it with `&CniInstallation` unchanged) and a running `Controller<PullThroughCache>`.

- [ ] **Step 1: Write the failing test**

In the `tests` module of `src/main.rs`, add:

```rust
    #[test]
    fn deletion_requested_works_for_the_pull_through_cache_kind_too() {
        let cache: PullThroughCache = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "PullThroughCache",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-25T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "spegel",
                "spegel": { "chartVersion": "0.7.4" }
            }
        }))
        .expect("cache should deserialize");

        assert_eq!(deletion_requested(&cache), Some(1));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin platform-controller`
Expected: compile error (`cannot find type \`PullThroughCache\`` and, once imported, a mismatched-types error because `deletion_requested` takes `&CniInstallation`).

- [ ] **Step 3: Update imports and make `deletion_requested` generic**

In `src/main.rs`:

1. After `use platform_controller::crd::CniInstallation;` add `use platform_controller::pull_through_cache::PullThroughCache;`.
2. After the existing `use platform_controller::reconciler::{...};` line add `use platform_controller::cache_reconciler;`.
3. Change the signature `fn deletion_requested(obj: &CniInstallation) -> Option<u64> {` to `fn deletion_requested<K: Resource>(obj: &K) -> Option<u64> {`. The body is unchanged.

- [ ] **Step 4: Add the second controller**

Replace the block that starts at `let controller = Controller::for_stream(installations, reader)` and ends just before `let mut sigterm = signal(SignalKind::terminate())?;` with:

```rust
    let controller = Controller::for_stream(installations, reader)
        .run(reconcile_with_finalizer, error_policy, context.clone())
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        });

    // The pull-through cache gets its own watcher, store and Controller, but the
    // same status-write/deletion predicate filter (see above) and the same
    // Context, so both loops share one leader lease.
    let cache_api: Api<PullThroughCache> = Api::all(client.clone());
    let (cache_reader, cache_writer) = reflector::store();
    let caches = watcher(cache_api, watcher::Config::default())
        .default_backoff()
        .reflect(cache_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let cache_controller = Controller::for_stream(caches, cache_reader)
        .run(
            cache_reconciler::reconcile_with_finalizer,
            cache_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled pull-through cache"),
                Err(err) => tracing::error!(error = %err, "pull-through cache reconcile failed"),
            }
        });

```

Then, in the `tokio::select!`, add a branch directly under `_ = controller => {}`:

```rust
        _ = cache_controller => {}
```

- [ ] **Step 5: Build and run the tests**

Run: `cargo build && cargo test --bin platform-controller`
Expected: builds; PASS (3 tests: the two existing `deletion_requested` tests and the new one).

- [ ] **Step 6: Run the whole suite and clippy**

Run: `cargo test && cargo clippy --all-targets`
Expected: all tests PASS (ignored tests reported as ignored). Clippy may report `result_large_err` on the new reconciler, matching the existing one; it must report no *other* new warning class. If it reports `collapsible_if` in `validate_spegel`, the let-chain from Task 2 Step 3 was not applied exactly as written.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "$(cat <<'EOF'
feat: run a PullThroughCache controller alongside the CNI controller

Second watcher, store and Controller with the same predicate filter and
the same Context, so both loops share one leader lease. deletion_requested
becomes generic over the resource kind.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Example manifest and its tests

**Files:**
- Create: `examples/pull-through-cache.yaml`
- Create: `tests/pull_through_cache_example.rs`

**Interfaces:**
- Consumes: `platform_controller::{cache_reconciler::validate, pull_through_cache::{PullThroughCache, build_values}, manifests::parse_manifests}`.
- Produces: `examples/pull-through-cache.yaml`, referenced by the README (Task 7) and the integration test (Task 8).

- [ ] **Step 1: Write the failing tests**

Create `tests/pull_through_cache_example.rs`:

```rust
use platform_controller::manifests::parse_manifests;
use platform_controller::pull_through_cache::{build_values, PullThroughCache};
use platform_controller::cache_reconciler;

const EXAMPLE: &str = "examples/pull-through-cache.yaml";

fn load() -> PullThroughCache {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a PullThroughCache")
}

#[test]
fn example_is_a_single_pull_through_cache_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "PullThroughCache");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let cache = load();

    assert_eq!(cache.metadata.name.as_deref(), Some("default"));
    cache_reconciler::validate("default", &cache.spec).expect("the example must be valid");
}

#[test]
fn example_names_its_registries_explicitly() {
    // Omitting `registries` mirrors every registry, private ones included, so the
    // starting point must make the choice visible.
    let spegel = load().spec.spegel;

    let registries = spegel.registries.expect("the example sets registries explicitly");
    assert!(registries.contains(&"docker.io".to_string()));
}

#[test]
fn example_chart_version_has_no_v_prefix() {
    // The OCI chart tag is `0.7.4`; `v0.7.4` is only the GitHub release tag and
    // does not exist on ghcr.io ("not found").
    let version = load().spec.spegel.chart_version;

    assert!(!version.starts_with('v'), "{version}");
}

#[test]
fn example_values_carry_the_talos_path_and_the_registries() {
    let spegel = load().spec.spegel;
    let values = build_values(&spegel);

    assert_eq!(
        values["spegel"]["containerdRegistryConfigPath"],
        "/etc/cri/conf.d/hosts"
    );
    assert_eq!(
        values["spegel"]["mirroredRegistries"],
        serde_json::json!(spegel.registries.unwrap())
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test pull_through_cache_example`
Expected: FAIL: `the example should exist: Os { code: 2, ... }` (file missing).

- [ ] **Step 3: Create the example**

`examples/pull-through-cache.yaml`:

```yaml
# Sample PullThroughCache for a self-hosted Talos Linux cluster: a Spegel
# peer-to-peer image mirror. Once one node has pulled an image, the other nodes
# fetch its layers from that peer instead of the upstream registry.
#
# BEFORE applying this, every node needs a one-time Talos machine-config change
# the controller cannot make for you (see
# docs/runbooks/pull-through-cache-verification.md, step 0):
#
#   machine:
#     files:
#       - path: /etc/cri/conf.d/20-customization.part
#         op: create
#         content: |
#           [plugins."io.containerd.cri.v1.images"]
#             discard_unpacked_layers = false
#
# Spegel publishes its registry on a hostPort, so apply this after the CNI is up
# (a CniInstallation that has reached Ready).
#
#   kubectl apply -f examples/pull-through-cache.yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: PullThroughCache
metadata:
  name: default
spec:
  platformKind: talos-linux
  provider: spegel
  spegel:
    # The OCI chart tag: NO "v" prefix, unlike the GitHub release tag.
    chartVersion: "0.7.4"
    # Omitting `registries` would mirror EVERY registry, private ones included.
    # An explicit list restricts mirroring to exactly these.
    registries:
      - docker.io
      - ghcr.io
      - quay.io
      - registry.k8s.io
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test pull_through_cache_example`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add examples/pull-through-cache.yaml tests/pull_through_cache_example.rs
git commit -m "$(cat <<'EOF'
docs: add a Talos PullThroughCache example and tests for it

The example lists registries explicitly (omitting them mirrors every
registry) and pins the OCI chart tag, which has no v prefix.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Deployment docs, runbook, RBAC ledger and spec corrections

**Files:**
- Modify: `deploy/README.md`
- Create: `docs/runbooks/pull-through-cache-verification.md`
- Modify: `docs/memory/rbac-cluster-admin-tradeoff.md`
- Modify: `docs/superpowers/specs/2026-09-25-pull-through-cache-design.md`

**Interfaces:** none (documentation). Consumes `examples/pull-through-cache.yaml` from Task 6.

- [ ] **Step 1: Update the apply sequence in `deploy/README.md`**

Replace the opening code block (the five-line `kubectl apply` sequence) with:

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cni-installation.yaml
```

Change the sentence "`crd.yaml` must be applied — and Established — first." to "`crd.yaml` (both CRDs) must be applied — and Established — first." Then append this section at the end of the file:

```markdown
## Pull-through image cache (optional)

`examples/pull-through-cache.yaml` is a `PullThroughCache` that installs
[Spegel](https://spegel.dev), a peer-to-peer image mirror, on Talos. Apply it
**after** the CNI is up (Spegel publishes its registry on a `hostPort`), and only
after doing the one-time Talos machine-config change described in
`docs/runbooks/pull-through-cache-verification.md` (step 0); the controller cannot
make that change for you.

Omitting `spec.spegel.registries` mirrors every registry, private ones included.
`spec.spegel.chartVersion` is the OCI chart tag and has no `v` prefix (`0.7.4`).

**Upgrading an existing install:** apply the new `deploy/crd.yaml` (and wait for
both CRDs to be Established) *before* rolling the controller image. A controller
that starts without the `PullThroughCache` CRD logs watch errors for it and
retries with backoff; it still reconciles `CniInstallation` normally.
```

- [ ] **Step 2: Create the runbook**

`docs/runbooks/pull-through-cache-verification.md`:

````markdown
# Verifying the Spegel pull-through cache on Talos

Manual acceptance for the `PullThroughCache` resource. Needs a Talos cluster with
at least two nodes and a working CNI (`CniInstallation` at `Ready`). The
Talos-in-Docker recipe in `tests/integration_talos.rs` works for steps 0-3; use
it with `--workers 2`.

## 0. Node prerequisite (once per node, before applying the CR)

Spegel serves layers containerd already unpacked, but Talos's containerd discards
unpacked layers by default. Patch every node's machine config:

```yaml
# spegel-talos-patch.yaml
machine:
  files:
    - path: /etc/cri/conf.d/20-customization.part
      op: create
      permissions: 0o644
      content: |
        [plugins."io.containerd.cri.v1.images"]
          discard_unpacked_layers = false
```

```sh
talosctl patch machineconfig --nodes <node-ip> --patch @spegel-talos-patch.yaml
```

Talos reports whether the change applied live or needs a reboot; if it needs one,
allow it. This is the one thing the controller cannot check: it reports only that
its own manifests were applied.

## 1. Apply

```sh
kubectl apply -f examples/pull-through-cache.yaml
kubectl get ptc default -o jsonpath='{.status.phase}{"\n"}'    # Ready
kubectl -n spegel get ds,pods -o wide                          # one pod per node, Running
```

`Ready` means the manifests were applied, not that Spegel is healthy on every
node; the DaemonSet's pods are the real signal.

## 2. Peer-to-peer serving

Pick an image that is not on any node yet, and two nodes (A and B):

```sh
kubectl run pull-a --image=<image> --overrides='{"spec":{"nodeName":"<node-A>"}}' --restart=Never
kubectl wait --for=condition=Ready pod/pull-a --timeout=120s
kubectl run pull-b --image=<image> --overrides='{"spec":{"nodeName":"<node-B>"}}' --restart=Never
kubectl wait --for=condition=Ready pod/pull-b --timeout=120s
```

Then confirm node B fetched from node A rather than upstream. Look at node B's
Spegel pod (`kubectl -n spegel logs <pod-on-B>`, and its `/metrics` on port 9090:
a mirror-requests counter labelled by source). The exact log line and metric name
come from the Spegel version in use (chart `0.7.4`); record what you observe
here so the next run does not have to guess.

## 3. Delete, and what nodes keep

Spegel removes the mirror config it wrote on each node through a **post-delete
Helm hook** (chart template `templates/post-delete-hook.yaml`: a `spegel-cleanup`
DaemonSet, a `spegel-cleanup-wait` Pod and a `spegel-cleanup` Service). The
controller renders with `--no-hooks`, so that hook does not run on delete.

```sh
kubectl delete ptc default          # returns once the finalizer clears
kubectl get ns spegel               # NotFound once terminating finishes
talosctl -n <node-ip> ls /etc/cri/conf.d/hosts     # do mirror configs remain?
```

Then, on a node, pull an image that no node has and that is under a mirrored
registry:

```sh
kubectl run after-delete --image=<uncached image> --restart=Never
kubectl wait --for=condition=Ready pod/after-delete --timeout=120s
```

- **Pull succeeds:** containerd fails open to the upstream registry when the
  local mirror is gone. Leftover mirror config is harmless; record that.
- **Pull hangs or fails:** it does not. The spec's open item is real and cleanup
  needs a node-cleanup step. Stop and revisit the spec before merging. As a
  manual workaround, apply only the hook objects:
  `helm template spegel oci://ghcr.io/spegel-org/helm-charts/spegel --version 0.7.4 --namespace spegel --show-only templates/post-delete-hook.yaml | kubectl apply -f -`,
  wait for the `spegel-cleanup` DaemonSet to finish, then delete them.

## Findings to record

After running this, write the outcomes of steps 2 and 3 (the peer-serving signal,
and fail-open yes/no) into `docs/memory/pull-through-cache-2026-09.md`.
````

- [ ] **Step 3: Add the ledger rows**

In `docs/memory/rbac-cluster-admin-tradeoff.md`, in the permission-ledger table, add these two rows directly below the existing `platform.rye.ninja` / `cniinstallations` row:

```markdown
| `platform.rye.ninja` | `pullthroughcaches`, `pullthroughcaches/status` | Controller itself | Its own second CRD (2026-09-25) — get/list/watch/update/patch, same as `cniinstallations`; finalizer updates go through `update`/`patch` on the main resource. |
| `""` (core), `apps` | `serviceaccounts`, `services`, `daemonsets`, `namespaces` | Spegel chart (`0.7.4`, rendered with `--no-hooks`) | Adds no kinds beyond what the controller already applies for Calico. The chart renders no Role/ClusterRole. Its post-delete hook objects (DaemonSet, Pod, Service) are never applied while `--no-hooks` is on. |
```

- [ ] **Step 4: Correct the spec with what the plan work found**

Apply these edits to `docs/superpowers/specs/2026-09-25-pull-through-cache-design.md`.

(a) In the validation list, after the bullet `- a \`registries\` entry is not a bare hostname (no scheme, no path, non-empty)`, add:

```markdown
- `registries` is present but empty (ambiguous; omit the field to mirror every registry)
```

(b) After the bullet beginning `- For \`talos-linux\` the controller always sets`, add:

```markdown
- `chartVersion` is the OCI chart tag, which has **no `v` prefix** (`0.7.4`); the GitHub release tag `v0.7.4` does not exist as a chart tag on `ghcr.io`.
```

(c) In the Reconcile section, after step 2's sentence ending `so the render-argument builder must produce both invocation forms.`, add:

```markdown
For OCI charts `helm template` also prints `Pulled:` and `Digest:` progress lines to stdout ahead of the manifests; `render_chart` strips them, since otherwise they parse as a bogus first manifest.
```

(d) Replace the bullet `- Finalizer, status and leader-gate glue (roughly 150 lines) is duplicated from \`reconciler.rs\` rather than extracted. The CNI path, live-verified after several cleanup fixes, is untouched apart from \`helm::render\`.` with:

```markdown
- Finalizer and status glue (roughly 150 lines) is duplicated from `reconciler.rs` rather than extracted. `leader_gate` and `wait_for_object_kind` are small and identical, so they are shared by making them `pub` in `reconciler.rs`. The CNI path, live-verified after several cleanup fixes, is otherwise untouched apart from `helm::render`.
```

(e) In the Cleanup section, replace the sentence `Spegel removes the mirror config it wrote on each node via a post-delete Helm hook.` with:

```markdown
Spegel removes the mirror config it wrote on each node via a post-delete Helm hook (confirmed in chart `0.7.4`: `templates/post-delete-hook.yaml` renders a `spegel-cleanup` DaemonSet, a `spegel-cleanup-wait` Pod and a `spegel-cleanup` Service).
```

(f) In "Unverified details", replace the bullet beginning `- Whether Spegel needs a working pod network` with:

```markdown
- Spegel's DaemonSet publishes its registry on `hostPort` 30020 (confirmed in the rendered chart `0.7.4`), which needs a CNI with hostPort support (Calico provides it). It only becomes usable after the CNI is up; the controller applies the manifests without waiting for that.
```

and in the same section replace `- The exact chart value names (\`spegel.mirroredRegistries\`, \`spegel.containerdRegistryConfigPath\`) against the pinned chart version.` with:

```markdown
- The chart value names `spegel.mirroredRegistries` and `spegel.containerdRegistryConfigPath` were confirmed against a real render of chart `0.7.4`; re-check them when the chart version is bumped.
```

- [ ] **Step 5: Verify the docs did not break anything**

Run: `cargo test --test bootstrap_manifests --test pull_through_cache_example`
Expected: PASS (nothing in these tests reads the edited docs; this guards against accidental edits to YAML files).

- [ ] **Step 6: Commit**

```bash
git add deploy/README.md docs/runbooks/pull-through-cache-verification.md docs/memory/rbac-cluster-admin-tradeoff.md docs/superpowers/specs/2026-09-25-pull-through-cache-design.md
git commit -m "$(cat <<'EOF'
docs: pull-through cache deployment notes, runbook and spec corrections

README apply sequence waits on both CRDs. The runbook holds the Talos
machine-config prerequisite and the live checks. The spec now records what
the real chart showed: no v prefix on the OCI tag, Pulled:/Digest: stdout
lines, the post-delete hook's location, and the hostPort dependency.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Live-cluster integration test and live verification

**Files:**
- Create: `tests/integration_pull_through_cache.rs`
- Create: `docs/memory/pull-through-cache-2026-09.md`
- Modify: `docs/memory/MEMORY.md`

**Interfaces:**
- Consumes: `platform_controller::pull_through_cache::PullThroughCache`, `platform_controller::crd::Phase`, `k8s_openapi::api::{apps::v1::DaemonSet, core::v1::Namespace}`.
- Produces: an `#[ignore]`d test, and the recorded live-verification outcome.

- [ ] **Step 1: Write the ignored integration test**

`tests/integration_pull_through_cache.rs`:

```rust
// Run manually against a real Talos cluster whose nodes already have the Spegel
// machine-config prerequisite and a working CNI. The setup is the one in
// tests/integration_talos.rs, with `--workers 2`, plus
// docs/runbooks/pull-through-cache-verification.md step 0. With the controller
// running and both CRDs Established:
//
//   kubectl apply -f examples/pull-through-cache.yaml
//   cargo test --test integration_pull_through_cache -- --ignored --nocapture
//
// The test deletes the PullThroughCache at the end, so re-apply the example to
// run it again. It does not try to prove peer-to-peer serving; that is a manual
// runbook step.

use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::crd::Phase;
use platform_controller::pull_through_cache::PullThroughCache;
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
#[ignore = "requires a real Talos cluster with a CNI and the Spegel prerequisite; see module docs"]
async fn spegel_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the Talos test cluster");

    let caches: Api<PullThroughCache> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let daemon_sets: Api<DaemonSet> = Api::namespaced(client.clone(), "spegel");

    eventually("PullThroughCache reaching Ready", Duration::from_secs(300), || async {
        caches
            .get("default")
            .await
            .ok()
            .and_then(|cache| cache.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    let listed = daemon_sets
        .list(&ListParams::default())
        .await
        .expect("should list DaemonSets in the spegel namespace");
    assert!(
        !listed.items.is_empty(),
        "Ready but no DaemonSet exists in the spegel namespace"
    );

    caches
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the PullThroughCache");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        caches.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the spegel namespace disappearing", Duration::from_secs(180), || async {
        namespaces.get_opt("spegel").await.expect("get_opt should succeed").is_none()
    })
    .await;
}
```

- [ ] **Step 2: Verify it compiles and is ignored by default**

Run: `cargo test --test integration_pull_through_cache`
Expected: `1 ignored`, no compile errors. (It cannot run without a cluster.)

- [ ] **Step 3: Commit the test**

```bash
git add tests/integration_pull_through_cache.rs
git commit -m "$(cat <<'EOF'
test: ignored live-cluster test for PullThroughCache apply and cleanup

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 4: MANUAL: run the live verification**

Requires a Talos cluster and a human. Follow `docs/runbooks/pull-through-cache-verification.md` steps 0-3, then run the ignored test from Step 1's header. The controller image used must include this branch's code (the bootstrap manifest points at `:latest`, which can lag the branch).

**Decision gate:** if step 3 of the runbook shows that pulls hang or fail after the CR is deleted (containerd does not fail open), stop. The spec's open item then applies: cleanup needs a node-cleanup step. Do not merge until the spec and reconciler are revised for that.

- [ ] **Step 5: Record what was learned**

Create `docs/memory/pull-through-cache-2026-09.md`, replacing each bracketed observation with what the live run showed:

```markdown
---
name: pull-through-cache-2026-09
description: PullThroughCache (Spegel) slice, 2026-09-25 - second CRD next to CniInstallation; non-obvious chart facts and the live-verification outcome
metadata:
  type: project
---

`PullThroughCache` (cluster-scoped singleton `default`, shortname `ptc`) installs Spegel on Talos. Spec: `docs/superpowers/specs/2026-09-25-pull-through-cache-design.md`; plan: `docs/superpowers/plans/2026-09-25-pull-through-cache.md`; live acceptance: `docs/runbooks/pull-through-cache-verification.md`. It is a parallel component (own reconciler, second `Controller` in `main.rs`, same leader lease), not a refactor of the CNI reconciler; extract a shared component framework only when a third component shows what is genuinely shared. kube-fledged pre-warming, an `external` provider and an `aws-ecr` provider are deliberately not built.

**Non-obvious facts (from rendering the real chart, not assumed):**
- The OCI chart tag has **no `v` prefix** (`0.7.4`); `v0.7.4` is only the GitHub release tag and 404s on ghcr.io.
- `helm template` on an `oci://` chart prints `Pulled:`/`Digest:` lines to **stdout**; unstripped they parse as a bogus first manifest. `helm::strip_oci_pull_preamble` removes them; the ignored test `helm::tests::spegel_chart_renders_parseable_manifests_with_our_values` pins it.
- `registries` maps to `spegel.mirroredRegistries`, whose default `[]` means **every registry**, so omitting it mirrors private registries too. An explicitly empty list is rejected as ambiguous.
- Spegel's DaemonSet publishes on `hostPort` 30020, so it needs the CNI up first (Calico provides hostPort). The reconciler applies without waiting for that.
- Node cleanup on delete is a **post-delete Helm hook** (`templates/post-delete-hook.yaml`), and the controller renders with `--no-hooks` (required: rendered hooks would run as live objects at install), so it does not run on delete.
- Running before the CNI (host-network mode) was considered and **dropped**: the chart has no `hostNetwork` value and hard-codes `--bootstrap-kind=dns` against cluster DNS, so it needs a post-render DaemonSet patch plus the HTTP bootstrapper; only nodes after the first would benefit and the cache dies with the cluster. Apply `PullThroughCache` after the CNI is `Ready`.
- The Talos node prerequisite (`/etc/cri/conf.d/20-customization.part` with `discard_unpacked_layers = false`) cannot be applied by the controller and cannot be verified by it; `Ready` means manifests applied only.

**Live-verified [date]:** [peer-to-peer serving: the signal observed (log line / metric name) and that node B was served by node A]. [After delete: containerd DID / DID NOT fail open to upstream; leftover mirror config under /etc/cri/conf.d/hosts was / was not present.] [Any environment prerequisite discovered.]

**How to apply:** when bumping the Spegel chart version, re-render it (`helm template ... oci://...`) and re-check `spegel.mirroredRegistries`, `spegel.containerdRegistryConfigPath`, the hostPort, and whether the post-delete hook changed; run the ignored real-chart test.
```

- [ ] **Step 6: Add the index line and commit**

Append to `docs/memory/MEMORY.md`:

```markdown
- [PullThroughCache (Spegel) slice](pull-through-cache-2026-09.md) — second CRD beside CniInstallation; OCI chart tag has no v, helm prints Pulled:/Digest: to stdout, omitted registries mirrors everything, post-delete hook not run under --no-hooks
```

```bash
git add docs/memory/pull-through-cache-2026-09.md docs/memory/MEMORY.md
git commit -m "$(cat <<'EOF'
docs: record the pull-through cache slice and its live-verification outcome

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage.** API/CRD shape and validation (Tasks 2, 4); typed-fields-win precedence and the Talos path (Task 2); reconcile flow, namespace, prune, status (Task 4); OCI render support (Task 1); cleanup in reverse ledger order (Task 4); second controller and shared lease (Task 5); both CRDs in `deploy/crd.yaml` and README sequencing (Tasks 3, 7); example (Task 6); runbook with the machine-config prerequisite, peer-serving check and delete/fail-open check (Task 7); RBAC ledger and memory (Tasks 7, 8); unit tests, example test, ignored integration test (Tasks 2, 4, 6, 8); the spec's open item (post-delete fail-open) is a manual decision gate in Task 8. Non-goals (kube-fledged, `external`/`aws-ecr`, Talos API, health condition, airgapped chart source) have no tasks, as intended.

**Deviations from the spec, all recorded back into it (Task 7 Step 4):** an explicitly empty `registries` list is rejected; `leader_gate` and `wait_for_object_kind` are shared via `pub` instead of duplicated; `Pulled:`/`Digest:` stripping; the chart tag has no `v`.

**Type consistency.** `render_chart`, `render_args`, `SPEGEL_CHART`, `SPEGEL_NAMESPACE` (Task 1) are consumed in Tasks 2 and 4 with the same signatures. `SpegelSpec` fields (`chart_version`, `registries`, `helm_values`) are used identically in Tasks 2, 4, 6. `validate` (Task 4) is called by name in Task 6's test. `crds::generated_yaml` (Task 3) is used by `crdgen` and the freshness test. `CacheReconcileError` / `ValidationError` in Task 4 are used only inside `cache_reconciler`; `main.rs` (Task 5) touches only `reconcile_with_finalizer` and `error_policy`.

**Known unverified points.** Only the live-cluster behavior: peer-to-peer serving, and whether containerd fails open to upstream after the CR is deleted (the spec's open item, gated in Task 8 Step 4).
