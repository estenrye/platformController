use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::pull_through_cache::{
    CacheSpecError, PullThroughCache, PullThroughCacheSpec, PullThroughCacheStatus,
};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME};
use kube::api::DynamicObject;
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "PullThroughCache {0:?} is ignored; this controller only reconciles the cluster-scoped \
         singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] CacheSpecError),
}

impl ValidationError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            ValidationError::UnsupportedPlatform(_) | ValidationError::UnsupportedName(_) => {
                "Unsupported"
            }
        }
    }
}

/// There is no provider check: `CacheProvider` has one variant and serde already
/// rejects any other value. Add one alongside the second provider.
pub fn validate(name: &str, spec: &PullThroughCacheSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::pull_through_cache::validate_spegel(&spec.spegel)?;
    Ok(())
}

/// The Spegel chart renders no `Namespace` object, so the controller synthesizes
/// one and applies it ahead of everything else. It is also tracked in
/// `status.appliedResources` so prune semantics stay consistent.
///
/// Spegel mounts the containerd socket and host paths, which Talos's default
/// `baseline` Pod Security Standard rejects, so the namespace is labelled
/// `privileged`.
pub fn spegel_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::SPEGEL_NAMESPACE,
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
pub enum CacheReconcileError {
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

impl CacheReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CacheReconcileError::Helm(_) => Some("RenderFailed"),
            CacheReconcileError::Manifest(_) => Some("InvalidManifest"),
            CacheReconcileError::Apply(_) => Some("ApplyFailed"),
            CacheReconcileError::Validation(_)
            | CacheReconcileError::Status(_)
            | CacheReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, CacheReconcileError> {
    let mut progress = crate::ledger::ReconcileProgress::default();
    let result = reconcile_inner(obj.clone(), ctx.clone(), &mut progress).await;
    if let Err(err) = &result
        && let Some(reason) = err.failure_reason()
    {
        record_failure(&obj, &ctx, &progress, reason, &err.to_string()).await;
    }
    result
}

/// Best effort: a failed status write is logged and never replaces the
/// reconcile's own error.
async fn record_failure(
    obj: &PullThroughCache,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<PullThroughCache> = kube::Api::all(ctx.client.clone());
    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();
    let ledger = crate::ledger::failure_ledger(&previous, progress.desired.as_deref());

    if let Err(err) = update_status(
        &api,
        &name,
        Phase::Failed,
        obj.metadata.generation,
        &obj.spec.spegel.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(cache = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CacheReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<PullThroughCache> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.spegel.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(cache = %name, error = %err, "validation failed");
        update_status(
            &api,
            &name,
            Phase::Failed,
            obj.metadata.generation,
            &chart_version,
            &previous,
            err.reason(),
            &err.to_string(),
        )
        .await?;
        return Err(CacheReconcileError::Validation(err));
    }
    tracing::info!(
        cache = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::pull_through_cache::build_values(&obj.spec.spegel);
    let rendered =
        crate::helm::render_chart(&crate::helm::SPEGEL_CHART, &chart_version, &values).await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::SPEGEL_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered spegel chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    let dropped = crate::pull_through_cache::drop_unbracketed_node_ip_mirror_targets(&mut objects);
    if dropped > 0 {
        tracing::info!(
            dropped,
            "dropped chart mirror targets that containerd cannot parse on IPv6 nodes"
        );
    }
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    // Everything this reconcile will apply is known now. Persist it before the
    // first apply so a failure, a crash or a leader change can never leave an
    // applied object out of the ledger cleanup acts on. Steady-state resyncs
    // add nothing, so they write nothing.
    let mut desired = vec![crate::apply::resource_ref(&spegel_namespace_object())];
    desired.extend(objects.iter().map(crate::apply::resource_ref));
    progress.desired = Some(desired.clone());
    if let Some(ledger) = crate::ledger::checkpoint_ledger(&previous, &desired) {
        tracing::info!(entries = ledger.len(), "checkpointing ledger before applying");
        update_status(
            &api,
            &name,
            Phase::Installing,
            obj.metadata.generation,
            &chart_version,
            &ledger,
            "Applying",
            "applying manifests",
        )
        .await?;
    }

    let mut applied = Vec::new();

    // The chart has no Namespace object of its own; create the target namespace
    // before anything that lives inside it.
    let namespace_ref =
        crate::apply::apply_object(&ctx.client, &spegel_namespace_object(), "platform-controller")
            .await?;
    applied.push(namespace_ref);

    for object in &objects {
        if crate::manifests::is_custom_resource(object) {
            wait_for_object_kind(&ctx.client, object).await?;
        }
        let reference =
            crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        if reference.kind == "CustomResourceDefinition" {
            crate::apply::wait_for_crd_established(
                &ctx.client,
                &reference.name,
                Duration::from_secs(10),
            )
            .await?;
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
    }

    debug_assert!(
        applied.iter().all(|reference| desired.contains(reference)),
        "applied an object that was not in the checkpointed ledger"
    );
    tracing::info!(cache = %name, phase = ?Phase::Ready, "updating status");
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
    _obj: Arc<PullThroughCache>,
    _err: &kube::runtime::finalizer::Error<CacheReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    api: &kube::Api<PullThroughCache>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CacheReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = PullThroughCacheStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CacheReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, CacheReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CacheReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(cache = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order: the namespace was applied first, so it goes last.
    // Spegel owns no resources that need a bounded removal wait.
    for reference in applied.iter().rev() {
        tracing::info!(
            cache = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(cache = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CacheReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all; see
    // `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<PullThroughCache> = kube::Api::all(ctx.client.clone());
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
    use crate::pull_through_cache::{CacheProvider, SpegelSpec};

    fn spec_with(platform_kind: PlatformKind) -> PullThroughCacheSpec {
        PullThroughCacheSpec {
            platform_kind,
            provider: CacheProvider::Spegel,
            spegel: SpegelSpec {
                chart_version: "v0.0.0-test".to_string(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn accepts_talos_linux_spegel_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_caches_not_named_default() {
        let err = validate("second", &spec_with(PlatformKind::TalosLinux))
            .expect_err("non-singleton names should be rejected");

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.spegel.chart_version = String::new();

        let err = validate("default", &spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn synthesized_namespace_is_privileged_and_named_spegel() {
        let object = spegel_namespace_object();
        let types = object.types.as_ref().expect("types should be set");
        let labels = object.metadata.labels.as_ref().expect("labels should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("spegel"));
        assert!(object.metadata.namespace.is_none());
        for mode in ["enforce", "audit", "warn"] {
            assert_eq!(
                labels.get(&format!("pod-security.kubernetes.io/{mode}")).map(String::as_str),
                Some("privileged")
            );
        }
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&spegel_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "spegel");
        assert_eq!(reference.namespace, "");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CacheReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CacheReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CacheReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "Namespace".to_string(),
            name: "spegel".to_string(),
            timeout: std::time::Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        // Validation already writes its own Failed status; a standby must never
        // write status at all.
        let validation = CacheReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CacheReconcileError::NotLeader.failure_reason(), None);
    }
}
