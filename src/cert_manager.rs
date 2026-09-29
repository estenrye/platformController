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
