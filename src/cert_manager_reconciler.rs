use crate::cert_manager::{
    CertManagerInstallation, CertManagerInstallationSpec, CertManagerInstallationStatus, CertManagerSpecError,
};
use crate::crd::{AppliedResourceRef, Condition, Phase, PlatformKind};
use crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME};
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
        "CertManagerInstallation {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] CertManagerSpecError),
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

pub fn validate(name: &str, spec: &CertManagerInstallationSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::cert_manager::validate_cert_manager(spec)?;
    Ok(())
}

/// The cert-manager chart renders no `Namespace` object, so the controller
/// synthesizes one and applies it ahead of everything else. It is also
/// tracked in `status.appliedResources` so prune semantics stay consistent.
///
/// Unlike Calico's `tigera-operator` namespace (hostPath CNI plugin binaries)
/// and Spegel's `spegel` namespace (containerd socket, host paths), this
/// namespace carries no `pod-security.kubernetes.io/*` labels at all:
/// cert-manager's controller/webhook/cainjector Deployments run with
/// `runAsNonRoot: true`, a seccomp profile, `allowPrivilegeEscalation: false`
/// and every capability dropped (live-verified 2026-09-28 rendering chart
/// v1.16.2) -- restricted-PSS-safe, so Talos's default `baseline` policy
/// admits them unmodified.
pub fn cert_manager_namespace_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": crate::helm::CERT_MANAGER_NAMESPACE,
        },
    }))
    .expect("static Namespace JSON deserializes into a DynamicObject")
}

/// Whether `objects` include at least one rendered `CustomResourceDefinition`.
///
/// `build_values` forces `crds.enabled: true` unconditionally, but that value
/// has no effect on cert-manager charts older than v1.15 (which used
/// `installCRDs` instead): `helm template` ignores unknown values rather than
/// erroring, so an old `chartVersion` silently renders zero CRDs. Checked
/// here, after parsing, so that misconfiguration surfaces as `Failed` /
/// `MissingCrds` instead of a cluster reporting `Ready` with cert-manager
/// Deployments running against no CRDs at all -- a non-functional install
/// with no error anywhere.
fn renders_expected_crds(objects: &[DynamicObject]) -> bool {
    objects
        .iter()
        .any(|object| object.types.as_ref().is_some_and(|types| types.kind == "CustomResourceDefinition"))
}

#[derive(thiserror::Error, Debug)]
pub enum CertManagerReconcileError {
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
        "chart version {0:?} rendered no CustomResourceDefinition objects; crds.enabled is \
         forced true but has no effect on charts older than v1.15 (which used installCRDs \
         instead) -- pin a newer chart version"
    )]
    NoCrdsRendered(String),
}

impl CertManagerReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CertManagerReconcileError::Helm(_) => Some("RenderFailed"),
            CertManagerReconcileError::Manifest(_) => Some("InvalidManifest"),
            CertManagerReconcileError::Apply(_) => Some("ApplyFailed"),
            CertManagerReconcileError::NoCrdsRendered(_) => Some("MissingCrds"),
            CertManagerReconcileError::Validation(_)
            | CertManagerReconcileError::Status(_)
            | CertManagerReconcileError::NotLeader => None,
        }
    }
}

/// Runs one reconcile. A failure after validation is written to `status`
/// (with a reason and the ledger of everything that may exist) before the
/// original error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, CertManagerReconcileError> {
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
    obj: &CertManagerInstallation,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CertManagerInstallation> = kube::Api::all(ctx.client.clone());
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
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CertManagerReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let api: kube::Api<CertManagerInstallation> = kube::Api::all(ctx.client.clone());
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
        return Err(CertManagerReconcileError::Validation(err));
    }
    tracing::info!(
        installation = %name,
        chart_version = %chart_version,
        "validation passed, reconciling"
    );

    let values = crate::cert_manager::build_values(&obj.spec);
    let rendered =
        crate::helm::render_chart(&crate::helm::CERT_MANAGER_CHART, &chart_version, &values).await?;
    tracing::info!(
        chart_version = %chart_version,
        namespace = crate::helm::CERT_MANAGER_NAMESPACE,
        rendered_bytes = rendered.len(),
        "rendered cert-manager chart"
    );

    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );

    if !renders_expected_crds(&objects) {
        return Err(CertManagerReconcileError::NoCrdsRendered(chart_version));
    }

    // Everything this reconcile will apply is known now. Persist it before the
    // first apply so a failure, a crash or a leader change can never leave an
    // applied object out of the ledger cleanup acts on. Steady-state resyncs
    // add nothing, so they write nothing.
    let mut desired = vec![crate::apply::resource_ref(&cert_manager_namespace_object())];
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
        &cert_manager_namespace_object(),
        "platform-controller",
    )
    .await?;
    applied.push(namespace_ref);

    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
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
    _obj: Arc<CertManagerInstallation>,
    _err: &kube::runtime::finalizer::Error<CertManagerReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<CertManagerInstallation>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), CertManagerReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CertManagerInstallationStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(CertManagerReconcileError::Status)?;

    Ok(())
}

pub async fn cleanup(
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, CertManagerReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch: kube::runtime::finalizer
    // treats any Ok here as "cleanup genuinely succeeded" and strips the
    // finalizer regardless of which replica returned it. See
    // `reconciler::cleanup` for the full reasoning.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CertManagerReconcileError::NotLeader);
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

    // Reverse ledger order: the namespace was applied first, so it goes last.
    // Nothing this component applies needs a bounded removal wait: it creates
    // no cert-manager.io custom resource itself.
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
    obj: Arc<CertManagerInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<CertManagerReconcileError>> {
    // A standby replica must never enter the finalizer state machine at all;
    // see `reconciler::reconcile_with_finalizer` for why.
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let api: kube::Api<CertManagerInstallation> = kube::Api::all(ctx.client.clone());
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

    fn spec_with(platform_kind: PlatformKind) -> CertManagerInstallationSpec {
        CertManagerInstallationSpec {
            platform_kind,
            chart_version: "v1.16.2".to_string(),
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
    fn synthesized_namespace_is_named_cert_manager_with_no_privileged_labels() {
        let object = cert_manager_namespace_object();
        let types = object.types.as_ref().expect("types should be set");

        assert_eq!(types.api_version, "v1");
        assert_eq!(types.kind, "Namespace");
        assert_eq!(object.metadata.name.as_deref(), Some("cert-manager"));
        assert!(object.metadata.namespace.is_none());
        // Unlike Calico's tigera-operator and Spegel's spegel namespace, this
        // one carries no pod-security labels at all: the chart's Deployments
        // are restricted-PSS-safe (live-verified, see Task 2's real-chart test).
        assert!(object.metadata.labels.is_none());
    }

    #[test]
    fn synthesized_namespace_is_tracked_as_an_applied_resource() {
        let reference = crate::apply::resource_ref(&cert_manager_namespace_object());

        assert_eq!(reference.api_version, "v1");
        assert_eq!(reference.kind, "Namespace");
        assert_eq!(reference.name, "cert-manager");
        assert_eq!(reference.namespace, "");
    }

    #[test]
    fn helm_errors_report_render_failed() {
        let err = CertManagerReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CertManagerReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CertManagerReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "Namespace".to_string(),
            name: "cert-manager".to_string(),
            timeout: Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        let validation = CertManagerReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CertManagerReconcileError::NotLeader.failure_reason(), None);
    }

    #[test]
    fn renders_expected_crds_is_false_when_no_crd_is_rendered() {
        // Live-verified: cert-manager charts older than v1.15 (which used
        // installCRDs instead of crds.enabled) silently render zero CRDs when
        // crds.enabled is set, since helm ignores unknown values rather than
        // erroring. This is the shape that scenario produces.
        let objects = crate::manifests::parse_manifests(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: cert-manager\n  namespace: cert-manager\n",
        )
        .expect("manifest should parse");

        assert!(!renders_expected_crds(&objects));
    }

    #[test]
    fn renders_expected_crds_is_true_when_a_crd_is_rendered() {
        let objects = crate::manifests::parse_manifests(
            "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: certificates.cert-manager.io\n",
        )
        .expect("manifest should parse");

        assert!(renders_expected_crds(&objects));
    }

    #[test]
    fn missing_crds_errors_report_missing_crds() {
        let err = CertManagerReconcileError::NoCrdsRendered("v1.14.7".to_string());

        assert_eq!(err.failure_reason(), Some("MissingCrds"));
    }
}
