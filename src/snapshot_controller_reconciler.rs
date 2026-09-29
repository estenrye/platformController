use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::reconciler::{leader_gate, wait_for_object_kind, Context, FINALIZER_NAME, SINGLETON_NAME};
use crate::snapshot_controller::{
    SnapshotController, SnapshotControllerSpec, SnapshotControllerSpecError, SnapshotControllerStatus,
    SNAPSHOT_CONTROLLER_ISSUER_NAME,
};
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
        "SnapshotController {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] SnapshotControllerSpecError),
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

pub fn validate(name: &str, spec: &SnapshotControllerSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::snapshot_controller::validate_snapshot_controller(spec)?;
    Ok(())
}

/// The chart renders no `Namespace` object, so the controller synthesizes
/// one and applies it ahead of everything else, the same as
/// `cert_manager_namespace_object`. No `pod-security.kubernetes.io/*`
/// labels: the chart's controller and webhook containers drop every
/// capability and run non-root (live-verified rendering chart 5.3.0),
/// though unlike cert-manager's chart they set no explicit `seccompProfile`
/// -- a stated assumption, confirmed or corrected live per the runbook.
pub fn snapshot_controller_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::SNAPSHOT_CONTROLLER_NAMESPACE,
        },
    }))
    .expect("static Namespace JSON deserializes into a DynamicObject")
}

/// A self-signed cert-manager `Issuer`, the exact recipe from the
/// snapshot-controller chart's own README, that the webhook's `Certificate`
/// (rendered by the chart via `webhook.tls.certManagerIssuerRef`) references
/// by name. Applied and tracked by this reconciler like any other object --
/// nothing here is jointly owned with `CertManagerInstallation`, which
/// installs cert-manager itself but deliberately configures no Issuer of its
/// own.
pub fn snapshot_controller_selfsigned_issuer_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "cert-manager.io/v1",
        "kind": "Issuer",
        "metadata": {
            "name": SNAPSHOT_CONTROLLER_ISSUER_NAME,
            "namespace": crate::helm::SNAPSHOT_CONTROLLER_NAMESPACE,
        },
        "spec": {
            "selfSigned": {},
        },
    }))
    .expect("static Issuer JSON deserializes into a DynamicObject")
}

/// Whether `objects` include at least one rendered `CustomResourceDefinition`.
///
/// `chartVersion` is user-typed and has no default: `helm template` ignores
/// unknown values keys rather than erroring, so a future chart version that
/// renames or removes `installCRDs` would render zero CRDs while everything
/// else (namespace, Issuer, RBAC, Deployments) still applies successfully.
/// Checked here, after parsing, so that misconfiguration surfaces as
/// `Failed` / `MissingCrds` instead of a cluster reporting `Ready` with the
/// entire reason this component exists -- the
/// `snapshot.storage.k8s.io`/`groupsnapshot.storage.k8s.io` CRDs --
/// silently absent. Mirrors `cert_manager_reconciler::renders_expected_crds`.
fn renders_expected_crds(objects: &[DynamicObject]) -> bool {
    objects
        .iter()
        .any(|object| object.types.as_ref().is_some_and(|types| types.kind == "CustomResourceDefinition"))
}

#[derive(thiserror::Error, Debug)]
pub enum SnapshotControllerReconcileError {
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
    #[error(
        "chart version {0:?} rendered no CustomResourceDefinition objects; installCRDs is \
         forced true but a future chart version that renames or removes that key would render \
         none while helm silently ignores the unknown key -- pin a chart version that still \
         supports installCRDs"
    )]
    NoCrdsRendered(String),
}

impl SnapshotControllerReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// `Apply` covers `ApplyError::KindNotAvailable` too -- the exact error
    /// `wait_for_object_kind` produces when `CertManagerInstallation` (and
    /// therefore the `Issuer`/`Certificate` CRDs) hasn't been applied yet --
    /// so that case surfaces as a retried `Failed`/`ApplyFailed`, with no
    /// new variant needed.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            SnapshotControllerReconcileError::Helm(_) => Some("RenderFailed"),
            SnapshotControllerReconcileError::Manifest(_) => Some("InvalidManifest"),
            SnapshotControllerReconcileError::Apply(_) => Some("ApplyFailed"),
            SnapshotControllerReconcileError::NoCrdsRendered(_) => Some("MissingCrds"),
            SnapshotControllerReconcileError::Validation(_)
            | SnapshotControllerReconcileError::Status(_)
            | SnapshotControllerReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status`
/// (with a reason and the ledger of everything that may exist) before the
/// original error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
) -> Result<Action, SnapshotControllerReconcileError> {
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
    obj: &SnapshotController,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<SnapshotController> = kube::Api::all(ctx.client.clone());
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
        &obj.spec.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(installation = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, SnapshotControllerReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<SnapshotController> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.chart_version.clone();

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
            err.reason(),
            &err.to_string(),
        )
        .await?;
        return Err(SnapshotControllerReconcileError::Validation(err));
    }
    tracing::info!(
        installation = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::snapshot_controller::build_values(&obj.spec);
    let rendered =
        crate::helm::render_chart(&crate::helm::SNAPSHOT_CONTROLLER_CHART, &chart_version, &values)
            .await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::SNAPSHOT_CONTROLLER_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered snapshot-controller chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    if !renders_expected_crds(&objects) {
        return Err(SnapshotControllerReconcileError::NoCrdsRendered(chart_version));
    }

    // Everything this reconcile will apply is known now, in apply order: the
    // target namespace, the self-signed Issuer the webhook's Certificate
    // references, then the chart's own rendered objects. Persist it before
    // the first apply so a failure, a crash or a leader change can never
    // leave an applied object out of the ledger cleanup acts on. Steady-state
    // resyncs add nothing, so they write nothing.
    let mut desired = vec![
        crate::apply::resource_ref(&snapshot_controller_namespace_object()),
        crate::apply::resource_ref(&snapshot_controller_selfsigned_issuer_object()),
    ];
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

    // The chart has no Namespace object of its own; create the target
    // namespace before anything that lives inside it.
    let namespace_ref = crate::apply::apply_object(
        &ctx.client,
        &snapshot_controller_namespace_object(),
        "platform-controller",
    )
    .await?;
    applied.push(namespace_ref);

    // The Issuer is a cert-manager.io custom resource: its CRD only exists
    // once CertManagerInstallation has been applied and reconciled. Waiting
    // here, rather than assuming it, is what turns "CertManagerInstallation
    // isn't applied yet" into a retried Failed/ApplyFailed status instead of
    // a hard, unretried discovery error.
    let issuer_object = snapshot_controller_selfsigned_issuer_object();
    wait_for_object_kind(&ctx.client, &issuer_object).await?;
    let issuer_ref =
        crate::apply::apply_object(&ctx.client, &issuer_object, "platform-controller").await?;
    applied.push(issuer_ref);

    for object in &objects {
        // The chart's own Certificate is a cert-manager.io custom resource
        // too and takes the same wait; every built-in kind this chart
        // renders (CRDs, RBAC, Service, Deployments) has an entry in
        // rank_for_kind's table, so is_custom_resource is only ever true for
        // Certificate here -- same pattern csi_reconciler.rs already uses
        // for StorageClass/CSIDriver.
        if crate::manifests::is_custom_resource(object) {
            wait_for_object_kind(&ctx.client, object).await?;
        }
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
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
    _obj: Arc<SnapshotController>,
    _err: &kube::runtime::finalizer::Error<SnapshotControllerReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<SnapshotController>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), SnapshotControllerReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = SnapshotControllerStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(SnapshotControllerReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
) -> Result<Action, SnapshotControllerReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(SnapshotControllerReconcileError::NotLeader);
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

    // Reverse ledger order: the namespace and Issuer were applied first, so
    // they go last. installCRDs: true means the chart's own CRDs are in this
    // ledger, so deleting them cascades away every VolumeSnapshot/
    // VolumeGroupSnapshot-family object in the cluster, not just this
    // component's own -- documented, not coded around, same as
    // CertManagerInstallation's cleanup.
    for reference in applied.iter().rev() {
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

pub async fn reconcile_with_finalizer(
    obj: Arc<SnapshotController>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<SnapshotControllerReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all;
    // see `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<SnapshotController> = kube::Api::all(ctx.client.clone());
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

    fn spec_with(platform_kind: PlatformKind) -> SnapshotControllerSpec {
        SnapshotControllerSpec {
            platform_kind,
            chart_version: "5.3.0".to_string(),
            helm_values: None,
        }
    }

    #[test]
    fn accepts_talos_linux_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let err = validate("second", &spec_with(PlatformKind::TalosLinux))
            .expect_err("non-singleton names should be rejected");

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.chart_version = String::new();

        let err = validate("default", &spec).expect_err("blank chartVersion is invalid");

        assert_eq!(err.reason(), "InvalidChartVersion");
    }

    #[test]
    fn synthesized_namespace_is_named_snapshot_controller_with_no_privileged_labels() {
        let object = snapshot_controller_namespace_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("snapshot-controller"));
        assert!(object.metadata.namespace.is_none());
        assert!(object.metadata.labels.is_none());
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&snapshot_controller_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "snapshot-controller");
        assert_eq!(reference.namespace, "");
    }

    #[test]
    fn selfsigned_issuer_object_is_named_and_namespaced_correctly() {
        let object = snapshot_controller_selfsigned_issuer_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "cert-manager.io/v1");
        assert_eq!(types.kind, "Issuer");
        assert_eq!(object.metadata.name.as_deref(), Some(SNAPSHOT_CONTROLLER_ISSUER_NAME));
        assert_eq!(object.metadata.namespace.as_deref(), Some("snapshot-controller"));
        assert_eq!(object.data["spec"]["selfSigned"], serde_json::json!({}));
    }

    #[test]
    fn selfsigned_issuer_object_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&snapshot_controller_selfsigned_issuer_object());

        assert_eq!(reference.api_version, "cert-manager.io/v1");
        assert_eq!(reference.kind, "Issuer");
        assert_eq!(reference.name, SNAPSHOT_CONTROLLER_ISSUER_NAME);
        assert_eq!(reference.namespace, "snapshot-controller");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = SnapshotControllerReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = SnapshotControllerReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_including_a_not_yet_registered_issuer_kind_report_apply_failed() {
        // The exact shape wait_for_object_kind's timeout produces when
        // CertManagerInstallation hasn't been applied yet: this must surface
        // as a retried Failed/ApplyFailed, not hang or panic.
        let err = SnapshotControllerReconcileError::Apply(crate::apply::ApplyError::KindNotAvailable {
            api_version: "cert-manager.io/v1".to_string(),
            kind: "Issuer".to_string(),
            timeout: Duration::from_secs(180),
            detail: "kind not registered yet".to_string(),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        let validation = SnapshotControllerReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(SnapshotControllerReconcileError::NotLeader.failure_reason(), None);
    }

    #[test]
    fn renders_expected_crds_is_false_when_no_crd_is_rendered() {
        // Mirrors cert_manager_reconciler's own guard: chartVersion is
        // user-typed and has no default, and helm ignores unknown values keys
        // rather than erroring, so a future chart version that renames or
        // removes installCRDs would render zero CRDs while everything else
        // still applies successfully. This is the shape that scenario
        // produces.
        let objects = crate::manifests::parse_manifests(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: snapshot-controller\n  namespace: snapshot-controller\n",
        )
        .expect("manifest should parse");

        assert!(!renders_expected_crds(&objects));
    }

    #[test]
    fn renders_expected_crds_is_true_when_a_crd_is_rendered() {
        let objects = crate::manifests::parse_manifests(
            "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: volumesnapshots.snapshot.storage.k8s.io\n",
        )
        .expect("manifest should parse");

        assert!(renders_expected_crds(&objects));
    }

    #[test]
    fn missing_crds_errors_report_missing_crds() {
        let err = SnapshotControllerReconcileError::NoCrdsRendered("5.2.0".to_string());

        assert_eq!(err.failure_reason(), Some("MissingCrds"));
    }
}
