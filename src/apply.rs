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
    #[error(
        "{api_version}/{kind} did not become available within {timeout:?} (last observed: {detail})"
    )]
    KindNotAvailable {
        api_version: String,
        kind: String,
        timeout: Duration,
        detail: String,
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

/// How long to wait for a custom resource's kind to be registered before
/// giving up. A first install on a CNI-less cluster includes the operator's
/// image pull and CRD registration, so this is deliberately generous.
pub const KIND_AVAILABLE_TIMEOUT: Duration = Duration::from_secs(180);

/// True when API discovery failed because the kind's CRD (and so its whole API
/// group/resource) is not registered, as opposed to a malformed reference or a
/// transient network failure.
pub fn is_missing_kind_error(err: &kube::Error) -> bool {
    match err {
        kube::Error::Api(status) => status.code == 404,
        kube::Error::Discovery(
            kube::error::DiscoveryError::MissingKind(_)
            | kube::error::DiscoveryError::MissingApiGroup(_)
            | kube::error::DiscoveryError::EmptyApiGroup(_),
        ) => true,
        _ => false,
    }
}

/// Polls API discovery until `api_version`/`kind` resolves. From Calico 3.32
/// the operator (not the chart) creates every CRD at startup, so the
/// controller cannot apply a CRD and wait on it; it can only wait for the kind
/// to appear. Every discovery call is wrapped in `timeout_at`: a wedged
/// connection would otherwise defeat the deadline (see
/// docs/memory/wait-for-crd-established.md).
pub async fn wait_for_kind_available(
    client: &kube::Client,
    api_version: &str,
    kind: &str,
    timeout: Duration,
) -> Result<(), ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: api_version.to_string(),
        kind: kind.to_string(),
    };
    let gvk = group_version_kind(&types);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut detail = "no attempt completed".to_string();

    loop {
        match tokio::time::timeout_at(deadline, kube::discovery::oneshot::pinned_kind(client, &gvk)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(err)) => {
                detail = if is_missing_kind_error(&err) {
                    "kind not registered yet".to_string()
                } else {
                    format!("discovery error: {err}")
                };
                tracing::debug!(api_version, kind, %detail, "waiting for kind to be registered");
            }
            Err(_) => {
                return Err(ApplyError::KindNotAvailable {
                    api_version: api_version.to_string(),
                    kind: kind.to_string(),
                    timeout,
                    detail,
                });
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ApplyError::KindNotAvailable {
                api_version: api_version.to_string(),
                kind: kind.to_string(),
                timeout,
                detail,
            });
        }

        let next_poll = tokio::time::Instant::now() + Duration::from_secs(2);
        tokio::time::sleep_until(next_poll.min(deadline)).await;
    }
}

async fn dynamic_api_for(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<Option<kube::Api<DynamicObject>>, ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: reference.api_version.clone(),
        kind: reference.kind.clone(),
    };
    let gvk = group_version_kind(&types);

    match kube::discovery::oneshot::pinned_kind(client, &gvk).await {
        Ok((api_resource, _caps)) => Ok(Some(if reference.namespace.is_empty() {
            kube::Api::all_with(client.clone(), &api_resource)
        } else {
            kube::Api::namespaced_with(client.clone(), &reference.namespace, &api_resource)
        })),
        // The kind's CRD (and therefore its whole API group/resource) has
        // already been removed from the cluster. During cleanup this means
        // the resource we were about to act on is unambiguously already
        // gone, not a real failure — without this, any retry after a CRD is
        // deleted would permanently wedge on rediscovering it. Any other
        // discovery error (a malformed reference, transient network
        // failure, etc.) still propagates.
        Err(err) if is_missing_kind_error(&err) => Ok(None),
        Err(source) => Err(ApplyError::Discovery {
            api_version: reference.api_version.clone(),
            kind: reference.kind.clone(),
            source,
        }),
    }
}

pub async fn delete_object(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<(), ApplyError> {
    let Some(api) = dynamic_api_for(client, reference).await? else {
        return Ok(());
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

pub async fn delete_and_wait_for_removal(
    client: &kube::Client,
    reference: &AppliedResourceRef,
    timeout: Duration,
) -> Result<(), ApplyError> {
    delete_object(client, reference).await?;

    if timeout.is_zero() {
        // A zero timeout is the documented opt-out for "don't block waiting
        // for removal" (see spec: cleanupTimeoutSeconds near 0). The delete
        // was already issued above; there's no meaningful bounded poll to
        // perform in a zero-length window, and tokio::time::timeout_at with
        // an already-elapsed deadline can never let a real networked call
        // complete, so treating this as a timeout failure would make the
        // documented escape hatch permanently fail instead of "proceed
        // immediately" as intended.
        return Ok(());
    }

    let Some(api) = dynamic_api_for(client, reference).await? else {
        return Ok(());
    };
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
    fn a_404_from_discovery_means_the_kind_is_not_registered() {
        let err = kube::Error::Api(Box::new(kube::core::Status {
            code: 404,
            ..Default::default()
        }));

        assert!(is_missing_kind_error(&err));
    }

    #[test]
    fn missing_kind_and_missing_group_discovery_errors_mean_not_registered() {
        for err in [
            kube::error::DiscoveryError::MissingKind("Installation".to_string()),
            kube::error::DiscoveryError::MissingApiGroup("operator.tigera.io".to_string()),
            kube::error::DiscoveryError::EmptyApiGroup("operator.tigera.io/v1".to_string()),
        ] {
            assert!(is_missing_kind_error(&kube::Error::Discovery(err)));
        }
    }

    #[test]
    fn other_api_errors_are_not_treated_as_missing_kind() {
        let err = kube::Error::Api(Box::new(kube::core::Status {
            code: 500,
            ..Default::default()
        }));

        assert!(!is_missing_kind_error(&err));
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
