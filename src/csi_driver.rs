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
