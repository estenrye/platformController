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
        assert_eq!(spec.openstack_cinder.storage_classes.default, DefaultStorageClass::Delete);
        assert!(spec.openstack_cinder.storage_classes.delete.parameters.is_empty());
        assert!(spec.openstack_cinder.storage_classes.retain.parameters.is_empty());
        assert!(spec.openstack_cinder.storage_classes.additional.is_empty());
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
    fn values_render_two_additional_storage_classes_into_separate_documents() {
        use serde::Deserialize;

        let mut spec = openstack_cinder();
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "csi-cinder-sc-az1".to_string(),
            reclaim_policy: ReclaimPolicy::Delete,
            parameters: std::collections::BTreeMap::from([("availability".to_string(), "az1".to_string())]),
            is_default: false,
        });
        spec.storage_classes.additional.push(AdditionalStorageClassSpec {
            name: "csi-cinder-sc-az2".to_string(),
            reclaim_policy: ReclaimPolicy::Retain,
            parameters: std::collections::BTreeMap::from([("availability".to_string(), "az2".to_string())]),
            is_default: false,
        });

        let values = build_values(&spec);
        let custom = values["storageClass"]["custom"].as_str().expect("custom is a string");
        let docs: Vec<serde_json::Value> = serde_yaml::Deserializer::from_str(custom)
            .map(|doc| serde_yaml::Value::deserialize(doc).expect("valid YAML"))
            .map(|v| serde_json::to_value(v).expect("YAML converts to JSON"))
            .collect();

        assert_eq!(docs.len(), 2, "{custom}");

        let az1 = docs
            .iter()
            .find(|doc| doc["metadata"]["name"] == "csi-cinder-sc-az1")
            .expect("the az1 document");
        assert_eq!(az1["reclaimPolicy"], "Delete");
        assert_eq!(az1["parameters"]["availability"], "az1");
        assert_eq!(az1["parameters"].as_object().unwrap().len(), 1, "{custom}");

        let az2 = docs
            .iter()
            .find(|doc| doc["metadata"]["name"] == "csi-cinder-sc-az2")
            .expect("the az2 document");
        assert_eq!(az2["reclaimPolicy"], "Retain");
        assert_eq!(az2["parameters"]["availability"], "az2");
        assert_eq!(az2["parameters"].as_object().unwrap().len(), 1, "{custom}");
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
    fn values_always_drop_the_incompatible_cacert_hostpath_mount() {
        // Talos's root filesystem is read-only and never creates /etc/cacert,
        // so the chart's default hostPath mount for it crashes both the node
        // and controller plugin containers outright (live-verified). Only the
        // required cloud-config Secret mount survives the override.
        let values = build_values(&openstack_cinder());

        assert_eq!(values["csi"]["plugin"]["volumes"], serde_json::json!([]));
        assert_eq!(
            values["csi"]["plugin"]["volumeMounts"],
            serde_json::json!([
                { "name": "cloud-config", "mountPath": "/etc/config", "readOnly": true },
            ])
        );
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
        spec.storage_classes.default = DefaultStorageClass::Retain;

        let values = build_values(&spec);

        assert_eq!(values["storageClass"]["delete"]["isDefault"], false);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], true);
    }

    #[test]
    fn default_storage_class_none_marks_neither_class_default() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::None;

        let values = build_values(&spec);

        assert_eq!(values["storageClass"]["delete"]["isDefault"], false);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], false);
    }

    #[test]
    fn typed_values_win_over_conflicting_helm_values_default_storage_class_delete() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::Delete;
        spec.helm_values = Some(serde_json::json!({
            "secret": { "create": true, "enabled": false, "hostMount": true, "name": "other" },
            "storageClass": { "delete": { "isDefault": false }, "retain": { "isDefault": true } },
            "csi": { "plugin": { "volumes": [{ "name": "cacert", "hostPath": { "path": "/etc/cacert" } }] } }
        }));

        let values = build_values(&spec);

        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["hostMount"], false);
        assert_eq!(values["secret"]["name"], "cloud-config");
        assert_eq!(values["storageClass"]["delete"]["isDefault"], true);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], false);
        assert_eq!(values["csi"]["plugin"]["volumes"], serde_json::json!([]));
    }

    #[test]
    fn typed_values_win_over_conflicting_helm_values_default_storage_class_retain() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::Retain;
        // helmValues claims the opposite class (delete) is default; the typed
        // field must still win, in the opposite direction from the delete case.
        spec.helm_values = Some(serde_json::json!({
            "secret": { "create": true, "enabled": false, "hostMount": true, "name": "other" },
            "storageClass": { "delete": { "isDefault": true }, "retain": { "isDefault": false } }
        }));

        let values = build_values(&spec);

        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["hostMount"], false);
        assert_eq!(values["secret"]["name"], "cloud-config");
        assert_eq!(values["storageClass"]["delete"]["isDefault"], false);
        assert_eq!(values["storageClass"]["retain"]["isDefault"], true);
    }

    #[test]
    fn typed_values_win_over_conflicting_helm_values_default_storage_class_none() {
        let mut spec = openstack_cinder();
        spec.storage_classes.default = DefaultStorageClass::None;
        // helmValues claims both classes are default; the typed field must still
        // win and clear both flags.
        spec.helm_values = Some(serde_json::json!({
            "secret": { "create": true, "enabled": false, "hostMount": true, "name": "other" },
            "storageClass": { "delete": { "isDefault": true }, "retain": { "isDefault": true } }
        }));

        let values = build_values(&spec);

        assert_eq!(values["secret"]["create"], false);
        assert_eq!(values["secret"]["enabled"], true);
        assert_eq!(values["secret"]["hostMount"], false);
        assert_eq!(values["secret"]["name"], "cloud-config");
        assert_eq!(values["storageClass"]["delete"]["isDefault"], false);
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
