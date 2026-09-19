use crate::crd::AppliedResourceRef;
use kube::api::DynamicObject;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use std::time::Duration;

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
    #[error("CRD {name} did not become Established within {timeout:?} (last observed: {detail})")]
    NotEstablished {
        name: String,
        timeout: Duration,
        detail: String,
    },
    #[error("{kind}/{name} was not removed within {timeout:?}")]
    NotDeleted {
        kind: String,
        name: String,
        timeout: Duration,
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

fn is_established(crd: &CustomResourceDefinition) -> bool {
    crd.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "Established" && condition.status == "True")
        })
        .unwrap_or(false)
}

fn describe_conditions(crd: &CustomResourceDefinition) -> String {
    crd.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .map(|c| format!("{}={}", c.type_, c.status))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "no conditions reported".to_string())
}

pub async fn wait_for_crd_established(
    client: &kube::Client,
    name: &str,
    timeout: Duration,
) -> Result<(), ApplyError> {
    let api: kube::Api<CustomResourceDefinition> = kube::Api::all(client.clone());
    let deadline = tokio::time::Instant::now() + timeout;
    let mut detail = "no status observed".to_string();

    loop {
        match tokio::time::timeout_at(deadline, api.get(name)).await {
            Ok(Ok(crd)) => {
                if is_established(&crd) {
                    return Ok(());
                }
                detail = describe_conditions(&crd);
            }
            Ok(Err(source)) => {
                tracing::debug!(crd = %name, error = %source, "failed to fetch CRD while waiting for Established");
                detail = format!("fetch error: {source}");
            }
            Err(_) => {
                return Err(ApplyError::NotEstablished {
                    name: name.to_string(),
                    timeout,
                    detail,
                });
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ApplyError::NotEstablished {
                name: name.to_string(),
                timeout,
                detail,
            });
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
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
        // force(): the controller is the sole owner of these fields. Without it a
        // one-off field-ownership conflict (e.g. a manual kubectl edit, or a
        // previous field manager name) wedges every future reconcile.
        &kube::api::PatchParams::apply(field_manager).force(),
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

async fn dynamic_api_for(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<kube::Api<DynamicObject>, ApplyError> {
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

    Ok(if reference.namespace.is_empty() {
        kube::Api::all_with(client.clone(), &api_resource)
    } else {
        kube::Api::namespaced_with(client.clone(), &reference.namespace, &api_resource)
    })
}

pub async fn delete_object(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<(), ApplyError> {
    let api = dynamic_api_for(client, reference).await?;

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

pub async fn delete_and_wait_for_removal(
    client: &kube::Client,
    reference: &AppliedResourceRef,
    timeout: Duration,
) -> Result<(), ApplyError> {
    delete_object(client, reference).await?;

    let api = dynamic_api_for(client, reference).await?;
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        match tokio::time::timeout_at(deadline, api.get_opt(&reference.name)).await {
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(_))) => {}
            Ok(Err(source)) => {
                tracing::debug!(
                    kind = %reference.kind,
                    name = %reference.name,
                    error = %source,
                    "failed to check whether resource was removed"
                );
            }
            Err(_) => {
                return Err(ApplyError::NotDeleted {
                    kind: reference.kind.clone(),
                    name: reference.name.clone(),
                    timeout,
                });
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ApplyError::NotDeleted {
                kind: reference.kind.clone(),
                name: reference.name.clone(),
                timeout,
            });
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
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

    #[test]
    fn resource_ref_recognizes_custom_resource_definition_kind() {
        let objects = crate::manifests::parse_manifests(
            "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: installations.operator.tigera.io\n",
        )
        .expect("manifest should parse");

        let reference = resource_ref(&objects[0]);

        assert_eq!(reference.kind, "CustomResourceDefinition");
    }

    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
        CustomResourceDefinition, CustomResourceDefinitionCondition, CustomResourceDefinitionStatus,
    };

    fn crd_with_conditions(conditions: Vec<CustomResourceDefinitionCondition>) -> CustomResourceDefinition {
        CustomResourceDefinition {
            status: Some(CustomResourceDefinitionStatus {
                conditions: Some(conditions),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn condition(type_: &str, status: &str) -> CustomResourceDefinitionCondition {
        CustomResourceDefinitionCondition {
            type_: type_.to_string(),
            status: status.to_string(),
            last_transition_time: None,
            message: None,
            observed_generation: None,
            reason: None,
        }
    }

    #[test]
    fn no_status_is_not_established() {
        let crd = CustomResourceDefinition::default();
        assert!(!is_established(&crd));
    }

    #[test]
    fn no_conditions_is_not_established() {
        let crd = crd_with_conditions(vec![]);
        assert!(!is_established(&crd));
    }

    #[test]
    fn established_condition_with_false_status_is_not_established() {
        let crd = crd_with_conditions(vec![condition("Established", "False")]);
        assert!(!is_established(&crd));
    }

    #[test]
    fn established_condition_with_true_status_is_established() {
        let crd = crd_with_conditions(vec![condition("Established", "True")]);
        assert!(is_established(&crd));
    }

    #[test]
    fn established_true_alongside_other_conditions_is_established() {
        let crd = crd_with_conditions(vec![
            condition("NamesAccepted", "True"),
            condition("Established", "True"),
        ]);
        assert!(is_established(&crd));
    }
}
