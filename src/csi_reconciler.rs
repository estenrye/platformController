use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::csi_driver::{CsiDriver, CsiDriverSpec, CsiDriverStatus, CsiSpecError, Driver};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "CsiDriver {given:?} is ignored; a CsiDriver with driver {driver:?} must be named {expected:?}"
    )]
    UnsupportedName {
        given: String,
        driver: Driver,
        expected: &'static str,
    },
    #[error(transparent)]
    Spec(#[from] CsiSpecError),
}

impl ValidationError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            ValidationError::UnsupportedPlatform(_) | ValidationError::UnsupportedName { .. } => {
                "Unsupported"
            }
        }
    }
}

/// There is no driver check beyond the name match: `Driver` has one variant
/// and serde already rejects any other value. Add one alongside the second
/// driver. Unlike the other three CRDs, `CsiDriver` is not a `name: default`
/// singleton: the name must equal the driver's own expected name instead, so
/// at most one CR can ever manage a given driver.
pub fn validate(name: &str, spec: &CsiDriverSpec) -> Result<(), ValidationError> {
    let expected = spec.driver.expected_name();
    if name != expected {
        return Err(ValidationError::UnsupportedName {
            given: name.to_string(),
            driver: spec.driver.clone(),
            expected,
        });
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::csi_driver::validate_openstack_cinder(&spec.openstack_cinder)?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum CsiReconcileError {
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

impl CsiReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CsiReconcileError::Helm(_) => Some("RenderFailed"),
            CsiReconcileError::Manifest(_) => Some("InvalidManifest"),
            CsiReconcileError::Apply(_) => Some("ApplyFailed"),
            CsiReconcileError::Validation(_)
            | CsiReconcileError::Status(_)
            | CsiReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(obj: Arc<CsiDriver>, ctx: Arc<Context>) -> Result<Action, CsiReconcileError> {
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
    obj: &CsiDriver,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CsiDriver> = kube::Api::all(ctx.client.clone());
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
        &obj.spec.openstack_cinder.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(csi_driver = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<CsiDriver>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CsiReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CsiDriver> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.openstack_cinder.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(csi_driver = %name, error = %err, "validation failed");
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
        return Err(CsiReconcileError::Validation(err));
    }
    tracing::info!(
        csi_driver = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    // Matching on `spec.driver` here, even with a single variant today, forces a
    // compiler error (non-exhaustive match) the moment a second `Driver` variant
    // is added, instead of that variant silently installing Cinder's chart. The
    // validation above already dispatches on the driver via `expected_name()`
    // and `validate_openstack_cinder`; this match covers the values-building and
    // chart-selection portion the same way.
    let rendered = match obj.spec.driver {
        Driver::OpenstackCinder => {
            let values = crate::csi_driver::build_values(&obj.spec.openstack_cinder);
            crate::helm::render_chart(
                &crate::helm::OPENSTACK_CINDER_CSI_CHART,
                &chart_version,
                &values,
            )
            .await?
        }
    };
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::CINDER_CSI_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered openstack-cinder-csi chart"
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
        // StorageClass and CSIDriver have no entry in `rank_for_kind`'s table, so
        // both fall into CUSTOM_RESOURCE_RANK and take this wait path too, even
        // though they're built-in kinds the API server already serves: the wait
        // resolves immediately for them, it's not a bug.
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
    tracing::info!(csi_driver = %name, phase = ?Phase::Ready, "updating status");
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
    _obj: Arc<CsiDriver>,
    _err: &kube::runtime::finalizer::Error<CsiReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    api: &kube::Api<CsiDriver>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CsiReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CsiDriverStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CsiReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(obj: Arc<CsiDriver>, ctx: Arc<Context>) -> Result<Action, CsiReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CsiReconcileError::NotLeader);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(csi_driver = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    // Reverse ledger order. There are no removal waits. Deleting this CR does
    // not delete already-provisioned Cinder volumes; PVCs or pods still
    // depending on them can be left with a stuck detach/unmount.
    for reference in applied.iter().rev() {
        tracing::info!(
            csi_driver = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(csi_driver = %name, "cleanup complete");
    Ok(Action::await_change())
}

pub async fn reconcile_with_finalizer(
    obj: Arc<CsiDriver>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CsiReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all; see
    // `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CsiDriver> = kube::Api::all(ctx.client.clone());
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
    use crate::crd::SecretNameRef;
    use crate::csi_driver::{Driver, OpenstackCinderSpec};

    fn spec_with(platform_kind: PlatformKind) -> CsiDriverSpec {
        CsiDriverSpec {
            platform_kind,
            driver: Driver::OpenstackCinder,
            openstack_cinder: OpenstackCinderSpec {
                chart_version: "2.36.5".to_string(),
                cloud_config_secret_ref: SecretNameRef {
                    name: "cloud-config".to_string(),
                },
                ..Default::default()
            },
        }
    }

    #[test]
    fn accepts_talos_linux_openstack_cinder_named_openstack_cinder() {
        assert!(validate("openstack-cinder", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_csi_drivers_not_named_for_their_driver() {
        for name in ["default", "cinder", "Openstack-Cinder", ""] {
            let err = validate(name, &spec_with(PlatformKind::TalosLinux))
                .expect_err("a name other than the driver's expected name should be rejected");

            assert!(matches!(
                &err,
                ValidationError::UnsupportedName { given, expected, .. }
                    if given == name && *expected == "openstack-cinder"
            ));
            assert_eq!(err.reason(), "Unsupported");
        }
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack_cinder.chart_version = String::new();
        let err = validate("openstack-cinder", &spec).expect_err("blank chartVersion is invalid");
        assert_eq!(err.reason(), "InvalidChartVersion");

        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.openstack_cinder.cloud_config_secret_ref.name = "Cloud_Config".to_string();
        let err = validate("openstack-cinder", &spec).expect_err("bad Secret name is invalid");
        assert_eq!(err.reason(), "InvalidSecretRef");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CsiReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CsiReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CsiReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "DaemonSet".to_string(),
            name: "openstack-cinder-csi-nodeplugin".to_string(),
            timeout: std::time::Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        // Validation already writes its own Failed status; a standby must never
        // write status at all.
        let validation = CsiReconcileError::Validation(ValidationError::UnsupportedName {
            given: "default".to_string(),
            driver: Driver::OpenstackCinder,
            expected: "openstack-cinder",
        });

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CsiReconcileError::NotLeader.failure_reason(), None);
    }
}
