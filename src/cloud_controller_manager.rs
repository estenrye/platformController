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
    #[error(
        "spec.openstack.helmValues must not set cloudConfig or cloudConfigContents: \
         secret.create is always false, so the chart never renders a Secret from them and \
         they have no effect; use spec.openstack.cloudConfigSecretRef instead"
    )]
    CloudConfigInHelmValues,
}

impl CcmSpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            CcmSpecError::EmptyChartVersion | CcmSpecError::ChartVersionHasWhitespace(_) => {
                "InvalidChartVersion"
            }
            CcmSpecError::InvalidSecretName(_) => "InvalidSecretRef",
            CcmSpecError::HelmValuesNotObject | CcmSpecError::CloudConfigInHelmValues => {
                "InvalidHelmValues"
            }
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
    if let Some(values) = &openstack.helm_values {
        if !values.is_object() {
            return Err(CcmSpecError::HelmValuesNotObject);
        }
        if values.get("cloudConfig").is_some() || values.get("cloudConfigContents").is_some() {
            return Err(CcmSpecError::CloudConfigInHelmValues);
        }
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
/// - `dnsPolicy` defaults (with the chart's own `hostNetwork: true`) to
///   `ClusterFirstWithHostNet`, which points the pod at the cluster DNS service
///   IP. That IP is unreachable before a CNI is up, and CoreDNS itself cannot
///   schedule until the CCM clears the `uninitialized` taint from every node --
///   the same deadlock `deploy/bootstrap.yaml` already documents for this
///   controller's own Deployment. `Default` (inheriting the node's resolv.conf)
///   is set unconditionally, so a `helmValues` passthrough can never reintroduce
///   the deadlock.
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
        "dnsPolicy": "Default",
    });

    crate::pull_through_cache::merge(&mut values, typed);
    values
}

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
    fn rejects_cloud_config_smuggled_through_helm_values() {
        // secret.create is always forced to false, so the chart's Secret
        // template (guarded by `if and .Values.secret.create .Values.secret.enabled`)
        // never renders: cloudConfig/cloudConfigContents have no effect on the
        // running CCM. A user who follows them would have their OpenStack
        // credentials sit, with no effect, in a cluster-scoped CR that is not a
        // Secret. Reject them outright rather than silently ignoring them.
        for key in ["cloudConfig", "cloudConfigContents"] {
            let mut spec = openstack();
            spec.helm_values = Some(serde_json::json!({ key: "[Global]\nauth-url=..." }));

            let err = validate_openstack(&spec).expect_err("cloudConfig via helmValues is invalid");

            assert_eq!(err, CcmSpecError::CloudConfigInHelmValues, "{key}");
            assert_eq!(err.reason(), "InvalidHelmValues");
        }
    }

    #[test]
    fn accepts_helm_values_that_do_not_touch_cloud_config() {
        let mut spec = openstack();
        spec.helm_values = Some(serde_json::json!({ "cluster": { "name": "prod" } }));

        assert_eq!(validate_openstack(&spec), Ok(()));
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
    fn values_always_use_the_default_dns_policy() {
        // The chart defaults to hostNetwork: true with dnsPolicy:
        // ClusterFirstWithHostNet, which points the pod at the cluster DNS
        // service IP. That IP is unreachable before a CNI is up, and CoreDNS
        // itself cannot schedule until the CCM clears the uninitialized taint
        // from every node -- a deadlock identical to the one deploy/bootstrap.yaml
        // already documents for this controller's own Deployment. `Default`
        // inherits the node's resolv.conf, which resolves the cloud's Keystone
        // endpoint with no pod network required.
        let values = build_values(&openstack());

        assert_eq!(values["dnsPolicy"], "Default");
    }

    #[test]
    fn typed_and_talos_values_win_over_conflicting_helm_values() {
        let mut spec = openstack();
        spec.helm_values = Some(serde_json::json!({
            "secret": { "create": true, "enabled": false, "name": "other" },
            "extraVolumes": [{ "name": "x" }],
            "extraVolumeMounts": [{ "name": "x" }],
            "dnsPolicy": "ClusterFirstWithHostNet"
        }));

        let values = build_values(&spec);

        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["name"], "cloud-config");
        assert_eq!(values["extraVolumes"], serde_json::json!([]));
        assert_eq!(values["extraVolumeMounts"], serde_json::json!([]));
        assert_eq!(values["dnsPolicy"], "Default");
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
