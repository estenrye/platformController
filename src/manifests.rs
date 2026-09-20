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

/// The rank bucket for any kind not explicitly enumerated below. Chart-managed
/// custom resources (e.g. `Installation`, `APIServer`) always fall here, since
/// they're applied last, after the CRDs and RBAC/workloads that define and run
/// them — and, symmetrically, are the first things cleanup deletes.
pub const CUSTOM_RESOURCE_RANK: u8 = 5;

pub fn rank_for_kind(kind: &str) -> u8 {
    match kind {
        "Namespace" => 0,
        "CustomResourceDefinition" => 1,
        "ServiceAccount" | "ClusterRole" | "ClusterRoleBinding" | "Role" | "RoleBinding" => 2,
        "ConfigMap" | "Secret" | "Service" | "ValidatingWebhookConfiguration" | "APIService" => 3,
        "Deployment" | "DaemonSet" => 4,
        _ => CUSTOM_RESOURCE_RANK,
    }
}

pub fn apply_rank(obj: &DynamicObject) -> u8 {
    let kind = obj.types.as_ref().map(|t| t.kind.as_str()).unwrap_or("");
    rank_for_kind(kind)
}

/// True for every kind that is not in `rank_for_kind`'s built-in table, i.e.
/// kinds assumed to be defined by a CRD and so applicable only once that CRD is
/// registered. From Calico 3.32 the operator, not the chart, registers them.
/// Built-in kinds that are simply absent from the table also match; the
/// follow-up kind wait then succeeds on its first poll.
pub fn is_custom_resource(obj: &DynamicObject) -> bool {
    apply_rank(obj) == CUSTOM_RESOURCE_RANK
}

pub fn sort_manifests(objects: &mut Vec<DynamicObject>) {
    objects.sort_by_key(apply_rank);
}

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

    #[test]
    fn rank_for_kind_places_custom_resources_in_the_highest_bucket() {
        assert_eq!(rank_for_kind("Namespace"), 0);
        assert_eq!(rank_for_kind("CustomResourceDefinition"), 1);
        assert_eq!(rank_for_kind("ServiceAccount"), 2);
        assert_eq!(rank_for_kind("ConfigMap"), 3);
        assert_eq!(rank_for_kind("Deployment"), 4);
        assert_eq!(rank_for_kind("Installation"), CUSTOM_RESOURCE_RANK);
        assert_eq!(rank_for_kind("APIServer"), CUSTOM_RESOURCE_RANK);
        assert_eq!(CUSTOM_RESOURCE_RANK, 5);
    }

    #[test]
    fn is_custom_resource_is_true_only_for_kinds_outside_the_builtin_ranks() {
        let objects = parse_manifests(SAMPLE_MANIFESTS).expect("manifests should parse");

        let flags: Vec<bool> = objects.iter().map(is_custom_resource).collect();

        // Namespace, Deployment, CustomResourceDefinition, Installation
        assert_eq!(flags, vec![false, false, false, true]);
    }
}
