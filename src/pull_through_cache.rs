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
    /// Upstream registries to mirror as URLs, e.g. `https://docker.io` (scheme
    /// `http` or `https`, host, optional numeric port, no path, no trailing
    /// slash; IPv6 literals are not supported). Omitted means the chart default,
    /// which mirrors every registry. An empty list is rejected as ambiguous.
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
    #[error("spec.spegel.chartVersion {0:?} has leading or trailing whitespace")]
    ChartVersionHasWhitespace(String),
    #[error("spec.spegel.registries is empty; omit the field to mirror every registry")]
    EmptyRegistries,
    #[error(
        "registries entry {0:?} is not a registry URL: expected http:// or https:// followed \
         by a host and optional numeric :port, with no path (for example https://docker.io)"
    )]
    InvalidRegistry(String),
    #[error("spec.spegel.helmValues must be a JSON object")]
    HelmValuesNotObject,
}

impl CacheSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            CacheSpecError::EmptyChartVersion | CacheSpecError::ChartVersionHasWhitespace(_) => {
                "InvalidChartVersion"
            }
            CacheSpecError::EmptyRegistries | CacheSpecError::InvalidRegistry(_) => {
                "InvalidRegistry"
            }
            CacheSpecError::HelmValuesNotObject => "InvalidHelmValues",
        }
    }
}

/// A registry URL: `http://` or `https://` followed by a host and an optional
/// numeric port, with no path, query or fragment: `https://docker.io`.
fn is_registry_url(entry: &str) -> bool {
    let Some(authority) = entry
        .strip_prefix("https://")
        .or_else(|| entry.strip_prefix("http://"))
    else {
        return false;
    };
    !authority.contains(['/', '?', '#']) && is_host_and_optional_port(authority)
}

/// A hostname with an optional numeric port: `docker.io`, `localhost:5000`.
fn is_host_and_optional_port(entry: &str) -> bool {
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
    if spegel.chart_version.trim() != spegel.chart_version {
        return Err(CacheSpecError::ChartVersionHasWhitespace(
            spegel.chart_version.clone(),
        ));
    }
    if let Some(registries) = &spegel.registries {
        if registries.is_empty() {
            return Err(CacheSpecError::EmptyRegistries);
        }
        if let Some(bad) = registries.iter().find(|entry| !is_registry_url(entry)) {
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
                "registries": ["https://docker.io", "https://ghcr.io"],
                "helmValues": { "resources": { "limits": { "memory": "128Mi" } } }
            }
        }))
        .expect("full spec should deserialize");

        assert_eq!(
            spec.spegel.registries,
            Some(vec![
                "https://docker.io".to_string(),
                "https://ghcr.io".to_string()
            ])
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
    fn rejects_chart_versions_with_leading_or_trailing_whitespace() {
        for bad in [" 0.7.4", "0.7.4 ", "0.7.4\n"] {
            let spec = SpegelSpec {
                chart_version: bad.to_string(),
                ..spegel()
            };

            let err = validate_spegel(&spec)
                .expect_err(&format!("{bad:?} should be rejected as a chart version"));

            assert_eq!(err, CacheSpecError::ChartVersionHasWhitespace(bad.to_string()));
            assert_eq!(err.reason(), "InvalidChartVersion");
        }
    }

    #[test]
    fn accepts_a_clean_chart_version() {
        let spec = SpegelSpec {
            chart_version: "0.7.4".to_string(),
            ..spegel()
        };

        assert_eq!(validate_spegel(&spec), Ok(()));
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
    fn accepts_registry_urls_with_optional_ports() {
        let spec = SpegelSpec {
            registries: Some(vec![
                "https://docker.io".to_string(),
                "https://registry.k8s.io".to_string(),
                "http://localhost:5000".to_string(),
                "https://my-registry.example.com:8443".to_string(),
            ]),
            ..spegel()
        };

        assert_eq!(validate_spegel(&spec), Ok(()));
    }

    #[test]
    fn rejects_registries_that_are_not_registry_urls() {
        for bad in [
            "docker.io",
            "docker.io:5000",
            "ftp://docker.io",
            "https://docker.io/library",
            "https://docker.io/",
            "https://",
            "https://docker.io:",
            "https://docker.io:port",
            "https://-docker.io",
            "https://docker.io.",
            "https://[fd00::1]:5000",
            "https://docker io",
            "https://docker.io?x=1",
            "HTTPS://docker.io",
            "https://user@docker.io",
            "https://docker.io:5000:1",
            "https://docker.io\t",
            "",
        ] {
            let spec = SpegelSpec {
                registries: Some(vec!["https://ghcr.io".to_string(), bad.to_string()]),
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
            registries: Some(vec![
                "https://docker.io".to_string(),
                "https://ghcr.io".to_string(),
            ]),
            ..spegel()
        };

        let values = build_values(&spec);

        assert_eq!(
            values["spegel"]["mirroredRegistries"],
            serde_json::json!(["https://docker.io", "https://ghcr.io"])
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
            registries: Some(vec!["https://ghcr.io".to_string()]),
            helm_values: Some(serde_json::json!({
                "spegel": {
                    "containerdRegistryConfigPath": "/somewhere/else",
                    "mirroredRegistries": ["https://docker.io"]
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
            serde_json::json!(["https://ghcr.io"])
        );
    }

    #[test]
    fn helm_values_passthrough_survives_when_registries_are_omitted() {
        let spec = SpegelSpec {
            helm_values: Some(serde_json::json!({
                "spegel": { "mirroredRegistries": ["https://quay.io"] }
            })),
            ..spegel()
        };

        let values = build_values(&spec);

        assert_eq!(
            values["spegel"]["mirroredRegistries"],
            serde_json::json!(["https://quay.io"])
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
