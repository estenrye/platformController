use crate::crd::AppliedResourceRef;
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

    let (api_resource, _caps) = kube::discovery::oneshot::pinned_kind(client, &gvk)
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

pub async fn delete_object(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<(), ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: reference.api_version.clone(),
        kind: reference.kind.clone(),
    };
    let gvk = group_version_kind(&types);

    let (api_resource, _caps) = kube::discovery::oneshot::pinned_kind(client, &gvk)
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

    match api
        .delete(&reference.name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(()),
        Err(source) => Err(ApplyError::Patch {
            kind: reference.kind.clone(),
            name: reference.name.clone(),
            source,
        }),
    }
}

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
}
