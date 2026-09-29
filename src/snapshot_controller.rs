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
    /// Whether to also install the CRD conversion webhook and the self-signed
    /// cert-manager `Issuer` group-snapshot support needs. Defaults to
    /// `false`: no CSI driver this controller supports today implements the
    /// CSI group-snapshot RPCs (`CreateVolumeGroupSnapshot` and friends --
    /// confirmed absent anywhere in `kubernetes/cloud-provider-openstack`'s
    /// `cinder-csi-plugin` source, live-verified 2026-09-29), and live-testing
    /// with this forced on found the webhook itself actively erroring
    /// (`unexpected conversion version from "groupsnapshot.storage.k8s.io/v1"
    /// to "...v1beta2"` on every attempt) rather than sitting idle -- worse
    /// than a no-op. A future platform or driver that does implement group
    /// snapshots can opt in explicitly once that's actually true.
    #[serde(default)]
    pub group_snapshots_enabled: bool,
    /// Free-form values merged into the chart's values. The controller's own
    /// typed values (`installCRDs`, `webhook.enabled`, `webhook.tls.*`,
    /// `controller.args.featureGates`) are overlaid afterwards, so they
    /// always win -- and `validate_snapshot_controller` rejects a
    /// `helmValues` attempt to set any of them outright, rather than
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
        "spec.helmValues must not set webhook.enabled: it is always derived from \
         spec.groupSnapshotsEnabled"
    )]
    HelmValuesSetWebhookEnabled,
    #[error(
        "spec.helmValues must not set webhook.tls.*: when spec.groupSnapshotsEnabled is true, \
         TLS is always wired to the self-signed cert-manager Issuer this component creates"
    )]
    HelmValuesSetWebhookTls,
    #[error(
        "spec.helmValues must not set controller.args.featureGates: it is always derived from \
         spec.groupSnapshotsEnabled"
    )]
    HelmValuesSetControllerArgsFeatureGates,
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
            | SnapshotControllerSpecError::HelmValuesSetWebhookTls
            | SnapshotControllerSpecError::HelmValuesSetControllerArgsFeatureGates => "InvalidHelmValues",
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
    if values
        .get("controller")
        .and_then(|controller| controller.get("args"))
        .and_then(|args| args.get("featureGates"))
        .is_some()
    {
        return Err(SnapshotControllerSpecError::HelmValuesSetControllerArgsFeatureGates);
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
/// defensive redundancy today.
///
/// `webhook.*` and `controller.args.featureGates` are entirely driven by
/// `spec.group_snapshots_enabled`:
/// - `false` (the default): `webhook.enabled: false` and
///   `featureGates: ""` -- live-verified against chart 5.3.0 to render
///   exactly one Deployment, no Service, no `Certificate`, and no
///   `spec.conversion` block on any of the six CRDs (defaults to `None`).
/// - `true`: `webhook.enabled: true`, `webhook.tls.autogenerate: false` plus
///   `certManagerIssuerRef` pointing at the self-signed `Issuer` the
///   reconciler creates (`SNAPSHOT_CONTROLLER_ISSUER_NAME`) -- the exact
///   recipe from the chart's own README -- and
///   `featureGates: "CSIVolumeGroupSnapshot=true"`.
pub fn build_values(spec: &SnapshotControllerSpec) -> serde_json::Value {
    let mut values = spec.helm_values.clone().unwrap_or_else(|| serde_json::json!({}));

    let typed = if spec.group_snapshots_enabled {
        serde_json::json!({
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
            "controller": {
                "args": {
                    "featureGates": "CSIVolumeGroupSnapshot=true",
                },
            },
        })
    } else {
        serde_json::json!({
            "installCRDs": true,
            "webhook": {
                "enabled": false,
            },
            "controller": {
                "args": {
                    "featureGates": "",
                },
            },
        })
    };

    crate::pull_through_cache::merge(&mut values, typed);
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn spec() -> SnapshotControllerSpec {
        SnapshotControllerSpec {
            platform_kind: crate::crd::PlatformKind::TalosLinux,
            chart_version: "5.3.0".to_string(),
            group_snapshots_enabled: false,
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
        assert!(!spec.group_snapshots_enabled, "omitted groupSnapshotsEnabled must default to false");
        assert!(spec.helm_values.is_none());
    }

    #[test]
    fn spec_deserializes_group_snapshots_enabled_when_set() {
        let spec: SnapshotControllerSpec = serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "chartVersion": "5.3.0",
            "groupSnapshotsEnabled": true
        }))
        .expect("spec should deserialize");

        assert!(spec.group_snapshots_enabled);
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
    fn rejects_helm_values_that_set_controller_args_feature_gates() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "controller": { "args": { "featureGates": "Foo=true" } } }));

        let err = validate_snapshot_controller(&s).expect_err("controller.args.featureGates via helmValues is invalid");

        assert_eq!(err, SnapshotControllerSpecError::HelmValuesSetControllerArgsFeatureGates);
        assert_eq!(err.reason(), "InvalidHelmValues");
    }

    #[test]
    fn accepts_helm_values_that_touch_other_controller_args() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "controller": { "args": { "httpEndpoint": ":9090" } } }));

        assert_eq!(validate_snapshot_controller(&s), Ok(()));
    }

    #[test]
    fn values_disable_webhook_and_the_group_snapshot_feature_gate_by_default() {
        let values = build_values(&spec());

        assert_eq!(values["installCRDs"], true);
        assert_eq!(values["webhook"]["enabled"], false);
        assert_eq!(values["controller"]["args"]["featureGates"], "");
    }

    #[test]
    fn values_enable_webhook_and_the_group_snapshot_feature_gate_when_requested() {
        let mut s = spec();
        s.group_snapshots_enabled = true;

        let values = build_values(&s);

        assert_eq!(values["installCRDs"], true);
        assert_eq!(values["webhook"]["enabled"], true);
        assert_eq!(values["webhook"]["tls"]["autogenerate"], false);
        assert_eq!(values["webhook"]["tls"]["certManagerIssuerRef"]["name"], SNAPSHOT_CONTROLLER_ISSUER_NAME);
        assert_eq!(values["webhook"]["tls"]["certManagerIssuerRef"]["kind"], "Issuer");
        assert_eq!(values["controller"]["args"]["featureGates"], "CSIVolumeGroupSnapshot=true");
    }

    #[test]
    fn helm_values_pass_through_alongside_the_typed_values() {
        let mut s = spec();
        s.helm_values = Some(serde_json::json!({ "controller": { "replicaCount": 2 }, "webhook": { "replicaCount": 2 } }));

        let values = build_values(&s);

        assert_eq!(values["controller"]["replicaCount"], 2);
        assert_eq!(values["webhook"]["replicaCount"], 2);
        assert_eq!(values["installCRDs"], true);
        assert_eq!(values["webhook"]["enabled"], false);
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
