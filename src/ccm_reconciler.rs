use crate::cloud_controller_manager::{
    CcmSpecError, CloudControllerManager, CloudControllerManagerSpec, CloudControllerManagerStatus,
};
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "CloudControllerManager {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] CcmSpecError),
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

/// There is no provider check: `CloudProvider` has one variant and serde already
/// rejects any other value. Add one alongside the second provider.
pub fn validate(name: &str, spec: &CloudControllerManagerSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::cloud_controller_manager::validate_openstack(&spec.openstack)?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum CcmReconcileError {
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

impl CcmReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CcmReconcileError::Helm(_) => Some("RenderFailed"),
            CcmReconcileError::Manifest(_) => Some("InvalidManifest"),
            CcmReconcileError::Apply(_) => Some("ApplyFailed"),
            CcmReconcileError::Validation(_)
            | CcmReconcileError::Status(_)
            | CcmReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
) -> Result<Action, CcmReconcileError> {
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
    obj: &CloudControllerManager,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CloudControllerManager> = kube::Api::all(ctx.client.clone());
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
        &obj.spec.openstack.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(ccm = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CcmReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CloudControllerManager> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.openstack.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(ccm = %name, error = %err, "validation failed");
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
        return Err(CcmReconcileError::Validation(err));
    }
    tracing::info!(
        ccm = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::cloud_controller_manager::build_values(&obj.spec.openstack);
    let rendered =
        crate::helm::render_chart(&crate::helm::OPENSTACK_CCM_CHART, &chart_version, &values)
            .await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::CCM_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered openstack cloud-controller-manager chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    // Everything this reconcile will apply is known now. Persist it before the
    // first apply so a failure, a crash or a leader change can never leave an
    // applied object out of the ledger cleanup acts on. Steady-state resyncs
    // add nothing, so they write nothing. No namespace is synthesized:
    // kube-system already exists and is not ours to delete.
    let desired: Vec<AppliedResourceRef> =
        objects.iter().map(crate::apply::resource_ref).collect();
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
    for object in &objects {
        // The chart renders neither CRDs nor custom resources today; these guard a
        // future chart bump that adds either.
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
    tracing::info!(ccm = %name, phase = ?Phase::Ready, "updating status");
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
    _obj: Arc<CloudControllerManager>,
    _err: &kube::runtime::finalizer::Error<CcmReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    api: &kube::Api<CloudControllerManager>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CcmReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CloudControllerManagerStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CcmReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
) -> Result<Action, CcmReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CcmReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(ccm = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order. The CCM owns no resources that need a bounded removal
    // wait. Deleting it does not undo node initialization: providerIDs and node
    // addresses stay, and existing cloud load balancers are not deleted.
    for reference in applied.iter().rev() {
        tracing::info!(
            ccm = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(ccm = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<CloudControllerManager>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CcmReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all; see
    // `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CloudControllerManager> = kube::Api::all(ctx.client.clone());
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
    use crate::cloud_controller_manager::{CloudProvider, OpenstackSpec, SecretNameRef};

    fn spec_with(platform_kind: PlatformKind) -> CloudControllerManagerSpec {
        CloudControllerManagerSpec {
            platform_kind,
            provider: CloudProvider::Openstack,
            openstack: OpenstackSpec {
                chart_version: "2.36.5".to_string(),
                cloud_config_secret_ref: SecretNameRef {
                    name: "cloud-config".to_string(),
                },
                ..Default::default()
            },
        }
    }

    #[test]
    fn accepts_talos_linux_openstack_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_managers_not_named_default() {
        for name in ["second", "Default", ""] {
            let err = validate(name, &spec_with(PlatformKind::TalosLinux))
                .expect_err("non-singleton names should be rejected");

            assert!(matches!(&err, ValidationError::UnsupportedName(n) if n == name));
            assert_eq!(err.reason(), "Unsupported");
        }
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack.chart_version = String::new();
        let err = validate("default", &spec).expect_err("blank chartVersion is invalid");
        assert_eq!(err.reason(), "InvalidChartVersion");

        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack.cloud_config_secret_ref.name = "Cloud_Config".to_string();
        let err = validate("default", &spec).expect_err("bad Secret name is invalid");
        assert_eq!(err.reason(), "InvalidSecretRef");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CcmReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CcmReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CcmReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "DaemonSet".to_string(),
            name: "openstack-cloud-controller-manager".to_string(),
            timeout: std::time::Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        // Validation already writes its own Failed status; a standby must never
        // write status at all.
        let validation = CcmReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CcmReconcileError::NotLeader.failure_reason(), None);
    }
}
