use crate::crd::{
    AppliedResourceRef, CniInstallation, CniInstallationSpec, CniInstallationStatus, CniProvider,
    Condition, Phase, PlatformKind,
};
use kube::api::DynamicObject;
use kube::runtime::controller::Action;
use kube::{Client, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

/// The only `CniInstallation` name this controller reconciles. The resource is a
/// cluster-scoped singleton: two of them would fight over the same cluster-wide
/// Calico objects and prune each other's applied resources.
pub const SINGLETON_NAME: &str = "default";

pub struct Context {
    pub client: Client,
    pub is_leader: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error("unsupported provider {0:?}, only Calico is supported")]
    UnsupportedProvider(CniProvider),
    #[error(
        "CniInstallation {0:?} is ignored; this controller only reconciles the cluster-scoped \
         singleton named \"default\""
    )]
    UnsupportedName(String),
}

pub fn validate(name: &str, spec: &CniInstallationSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    if spec.provider != CniProvider::Calico {
        return Err(ValidationError::UnsupportedProvider(spec.provider.clone()));
    }
    Ok(())
}

fn leader_gate(is_leader: &std::sync::atomic::AtomicBool) -> Option<Action> {
    if is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        None
    } else {
        Some(Action::requeue(Duration::from_secs(15)))
    }
}

/// The tigera-operator chart renders no `Namespace` object, so the controller
/// synthesizes one and applies it ahead of everything else. It is also tracked in
/// `status.appliedResources` so prune semantics stay consistent: if the namespace
/// is ever pruned, the next reconcile recreates it.
///
/// Talos enforces the `baseline` Pod Security Standard by default in every
/// namespace but kube-system, and the operator mounts a hostPath, so the
/// namespace is labelled `privileged`.
pub fn tigera_operator_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::TIGERA_OPERATOR_NAMESPACE,
            "labels": {
                "pod-security.kubernetes.io/enforce": "privileged",
                "pod-security.kubernetes.io/audit": "privileged",
                "pod-security.kubernetes.io/warn": "privileged",
            },
        },
    }))
    .expect("static Namespace JSON deserializes into a DynamicObject")
}

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Helm(#[from] crate::helm::HelmError),
    #[error(transparent)]
    Manifest(#[from] crate::manifests::ManifestError),
    #[error(transparent)]
    Apply(#[from] crate::apply::ApplyError),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
    #[error("not the leader; standing down")]
    NotLeader,
}

pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CniInstallation> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.calico.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(installation = %name, error = %err, "validation failed");
        update_status(
            &api,
            &name,
            Phase::Failed,
            obj.metadata.generation,
            &chart_version,
            &previous,
            "Unsupported",
            &err.to_string(),
        )
        .await?;
        return Err(ReconcileError::Validation(err));
    }
    tracing::info!(
        installation = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let rendered = crate::helm::render(&obj.spec.calico).await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::TIGERA_OPERATOR_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered tigera-operator chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    let mut applied = Vec::new();

    // The chart has no Namespace object of its own; create the target namespace
    // before anything that lives inside it.
    let namespace = tigera_operator_namespace_object();
    let namespace_ref =
        crate::apply::apply_object(&ctx.client, &namespace, "platform-controller").await?;
    tracing::debug!(
        namespace = crate::helm::TIGERA_OPERATOR_NAMESPACE,
        "applied target namespace"
    );
    applied.push(namespace_ref);

    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        if reference.kind == "CustomResourceDefinition" {
            crate::apply::wait_for_crd_established(
                &ctx.client,
                &reference.name,
                std::time::Duration::from_secs(10),
            )
            .await?;
            tracing::debug!(crd = %reference.name, "CRD established");
        }
        applied.push(reference);
    }
    tracing::info!(applied_count = applied.len(), "applied all objects");

    let stale = crate::apply::resources_to_prune(&previous, &applied);
    let pruned_count = stale.len();
    for reference in stale {
        crate::apply::delete_object(&ctx.client, &reference).await?;
    }
    if pruned_count > 0 {
        tracing::info!(pruned_count, "pruned resources no longer rendered");
    } else {
        tracing::debug!("nothing to prune");
    }

    tracing::info!(installation = %name, phase = ?Phase::Ready, "updating status");
    update_status(
        &api,
        &name,
        Phase::Ready,
        obj.metadata.generation,
        &chart_version,
        &applied,
        "Applied",
        "manifests applied successfully",
    )
    .await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy(
    _obj: Arc<CniInstallation>,
    _err: &kube::runtime::finalizer::Error<ReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<CniInstallation>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), ReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CniInstallationStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(ReconcileError::Status)?;

    Ok(())
}

fn partition_for_cleanup(
    resources: &[AppliedResourceRef],
) -> (Vec<AppliedResourceRef>, Vec<AppliedResourceRef>) {
    resources
        .iter()
        .cloned()
        .partition(|resource| crate::manifests::rank_for_kind(&resource.kind) == crate::manifests::CUSTOM_RESOURCE_RANK)
}

pub async fn cleanup(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the finalizer
    // regardless of which replica returned it. reconcile_with_finalizer already
    // gates non-leaders out before entering the finalizer machinery at all, making
    // this unreachable today — returning Err here (never Ok) means no future call
    // site can reintroduce the exact race d2d87e3 fixed.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(installation = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    let (custom_resources, infra) = partition_for_cleanup(&applied);
    let timeout = Duration::from_secs(u64::from(obj.spec.cleanup_timeout_seconds));

    for reference in custom_resources.iter().rev() {
        tracing::info!(
            installation = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting provider-managed resource and waiting for removal"
        );
        crate::apply::delete_and_wait_for_removal(&ctx.client, reference, timeout).await?;
    }

    for reference in infra.iter().rev() {
        tracing::info!(
            installation = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(installation = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub const FINALIZER_NAME: &str = "platform.rye.ninja/cleanup";

pub async fn reconcile_with_finalizer(
    obj: Arc<CniInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<ReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all.
    // `kube::runtime::finalizer` treats any `Ok` returned from the `Cleanup`
    // event as "cleanup genuinely succeeded" and immediately strips the
    // finalizer — it has no way to know the `Ok` came from a standby's
    // leader_gate short-circuit rather than real work. Gating here, before
    // the finalizer dispatch, means only the leader ever produces an `Ok`
    // for either event, so only real completions ever affect finalizer
    // state. (`reconcile`/`cleanup` each still call `leader_gate` too, as
    // defense in depth — harmless since it can now only ever see `is_leader
    // == true` by the time either is reached.)
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CniInstallation> = kube::Api::all(ctx.client.clone());
    kube::runtime::finalizer(&api, FINALIZER_NAME, obj, |event| async move {
        match event {
            kube::runtime::finalizer::Event::Apply(obj) => reconcile(obj, ctx).await,
            kube::runtime::finalizer::Event::Cleanup(obj) => cleanup(obj, ctx).await,
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{CalicoSpec, CniInstallationSpec, CniProvider, PlatformKind};

    fn spec_with(platform_kind: PlatformKind, provider: CniProvider) -> CniInstallationSpec {
        CniInstallationSpec {
            platform_kind,
            provider,
            calico: CalicoSpec {
                chart_version: "v3.29.1".to_string(),
                bgp_enabled: false,
                api_server_enabled: false,
                ip_pools: vec![],
                node_address_autodetection_v6_cidrs: vec![],
            },
            cleanup_timeout_seconds: 60,
        }
    }

    #[test]
    fn accepts_talos_linux_calico() {
        let spec = spec_with(PlatformKind::TalosLinux, CniProvider::Calico);
        assert!(validate("default", &spec).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let spec = spec_with(PlatformKind::TalosLinux, CniProvider::Calico);

        let err = validate("second", &spec).expect_err("non-singleton names should be rejected");

        assert!(matches!(err, ValidationError::UnsupportedName(name) if name == "second"));
    }

    #[test]
    fn synthesized_namespace_object_targets_tigera_operator() {
        let object = tigera_operator_namespace_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("tigera-operator"));
        assert!(object.metadata.namespace.is_none());
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&tigera_operator_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "tigera-operator");
        assert_eq!(reference.namespace, "");
    }

    #[test]
    fn leader_gate_returns_requeue_when_not_leader() {
        let is_leader = std::sync::atomic::AtomicBool::new(false);
        assert!(leader_gate(&is_leader).is_some());
    }

    #[test]
    fn leader_gate_returns_none_when_leader() {
        let is_leader = std::sync::atomic::AtomicBool::new(true);
        assert!(leader_gate(&is_leader).is_none());
    }

    fn applied_resource(kind: &str, name: &str) -> AppliedResourceRef {
        AppliedResourceRef {
            api_version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn partition_for_cleanup_separates_custom_resources_from_infra() {
        let namespace = applied_resource("Namespace", "tigera-operator");
        let crd = applied_resource("CustomResourceDefinition", "installations.operator.tigera.io");
        let deployment = applied_resource("Deployment", "tigera-operator");
        let installation = applied_resource("Installation", "default");

        let (custom_resources, infra) = partition_for_cleanup(&[
            namespace.clone(),
            crd.clone(),
            deployment.clone(),
            installation.clone(),
        ]);

        assert_eq!(custom_resources, vec![installation]);
        assert_eq!(infra, vec![namespace, crd, deployment]);
    }

    #[test]
    fn partition_for_cleanup_handles_no_custom_resources() {
        let namespace = applied_resource("Namespace", "tigera-operator");

        let (custom_resources, infra) = partition_for_cleanup(&[namespace.clone()]);

        assert!(custom_resources.is_empty());
        assert_eq!(infra, vec![namespace]);
    }

    #[test]
    fn partition_for_cleanup_handles_empty_input() {
        let (custom_resources, infra) = partition_for_cleanup(&[]);

        assert!(custom_resources.is_empty());
        assert!(infra.is_empty());
    }
}
