use crate::apiserver_probe::{delete_canary, ApiserverProbe, KubeApiserverProbe};
use crate::crd::{Condition, PlatformKind};
use crate::encryption_verdict::{all_clean, derive, Derivation};
use crate::etcd_encryption::{
    target_prefix, EncryptionPhase, EtcdEncryption, EtcdEncryptionSpec, EtcdEncryptionSpecError,
    EtcdEncryptionStatus, RewriteMode, RewriteProgress,
};
use crate::secret_rewrite::{rewrite_page, KubeSecretStore};
use crate::reconciler::{leader_gate, Context, SINGLETON_NAME};
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

/// The finalizer v0.1.11 added to every EtcdEncryption. This controller adds
/// none and owns nothing to clean up, so it removes this one (and only this one).
pub const LEGACY_FINALIZER: &str = "platform.rye.ninja/cleanup";

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error(
        "EtcdEncryption {0:?} is ignored; this controller only reconciles the \
         cluster-scoped singleton named \"default\""
    )]
    UnsupportedName(String),
    #[error(transparent)]
    Spec(#[from] EtcdEncryptionSpecError),
}

impl ValidationError {
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            ValidationError::UnsupportedPlatform(_) | ValidationError::UnsupportedName(_) => "Unsupported",
        }
    }
}

pub fn validate(name: &str, spec: &EtcdEncryptionSpec) -> Result<(), ValidationError> {
    if name != SINGLETON_NAME {
        return Err(ValidationError::UnsupportedName(name.to_string()));
    }
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    crate::etcd_encryption::validate_etcd_encryption(spec)?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum EtcdEncryptionReconcileError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("kubernetes API call failed: {0}")]
    Api(#[source] kube::Error),
    /// The probe could not be built or the rewrite could not list Secrets.
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
    #[error("not the leader; standing down")]
    NotLeader,
}

impl EtcdEncryptionReconcileError {
    /// The `Ready=False` reason to report, or `None` when this error must not
    /// overwrite status. Exhaustive on purpose: a new variant forces a decision.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            EtcdEncryptionReconcileError::Api(_) => Some("ApiCallFailed"),
            EtcdEncryptionReconcileError::Verification(_) => Some("VerificationFailed"),
            EtcdEncryptionReconcileError::Validation(_)
            | EtcdEncryptionReconcileError::Status(_)
            | EtcdEncryptionReconcileError::NotLeader => None,
        }
    }
}

pub fn condition(type_: &str, ok: bool, reason: &str, message: &str, generation: Option<i64>) -> Condition {
    Condition {
        type_: type_.to_string(),
        status: if ok { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    }
}

/// The finalizer list without the v0.1.11 finalizer, or `None` if it is absent.
pub fn strip_finalizer(finalizers: &[String]) -> Option<Vec<String>> {
    finalizers
        .iter()
        .any(|f| f == LEGACY_FINALIZER)
        .then(|| finalizers.iter().filter(|f| *f != LEGACY_FINALIZER).cloned().collect())
}

pub async fn write_status(
    api: &Api<EtcdEncryption>,
    name: &str,
    status: &EtcdEncryptionStatus,
) -> Result<(), EtcdEncryptionReconcileError> {
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(EtcdEncryptionReconcileError::Status)?;
    Ok(())
}

/// `Err(NotLeader)` unless this replica still holds the lease. Checked before
/// every status write: a reconcile can outlive the leadership it started with.
pub fn still_leader(is_leader: &std::sync::atomic::AtomicBool) -> Result<(), EtcdEncryptionReconcileError> {
    match leader_gate(is_leader) {
        Some(_) => Err(EtcdEncryptionReconcileError::NotLeader),
        None => Ok(()),
    }
}

/// `write_status`, but only while still the leader; a standby never writes.
async fn write_status_as_leader(
    api: &Api<EtcdEncryption>,
    name: &str,
    status: &EtcdEncryptionStatus,
    is_leader: &std::sync::atomic::AtomicBool,
) -> Result<(), EtcdEncryptionReconcileError> {
    still_leader(is_leader)?;
    write_status(api, name, status).await
}

/// A CR being deleted gets no probes, no status and no rewrite.
pub fn being_deleted(obj: &EtcdEncryption) -> bool {
    obj.metadata.deletion_timestamp.is_some()
}

/// Resets status to "cannot vouch for the cluster": never leaves a previously
/// derived safe phase or old per-node evidence behind.
pub fn reset_unverified(
    status: &mut EtcdEncryptionStatus,
    generation: Option<i64>,
    reason: &str,
    message: &str,
) {
    status.phase = EncryptionPhase::Observing;
    status.nodes = vec![];
    status.legacy_prefixes = vec![];
    status.rewrite = RewriteProgress::default();
    status.observed_generation = generation.unwrap_or(0);
    status.conditions = vec![condition("Ready", false, reason, message, generation)];
}

/// Active phases are re-checked quickly; settled ones slowly (the reader check
/// lists every Secret on every control-plane apiserver, so it is not cheap).
pub fn requeue_for(phase: EncryptionPhase) -> Duration {
    match phase {
        EncryptionPhase::Observing | EncryptionPhase::Migrating => Duration::from_secs(30),
        EncryptionPhase::NotConfigured | EncryptionPhase::ReadyToRemoveLegacy | EncryptionPhase::Verified => {
            Duration::from_secs(600)
        }
    }
}

/// `Ready` is true only when the cluster is `Verified`.
pub fn ready_condition_for(derivation: &Derivation, generation: Option<i64>) -> Condition {
    condition(
        "Ready",
        derivation.phase == EncryptionPhase::Verified,
        &format!("{:?}", derivation.phase),
        &derivation.reason,
        generation,
    )
}

/// The rewrite runs only when the derivation says so for the `Migrating` phase
/// AND the spec enables it. `derive` already requires both; this is the second lock.
pub fn should_run_rewrite(spec: &EtcdEncryptionSpec, derivation: &Derivation) -> bool {
    derivation.run_rewrite
        && derivation.phase == EncryptionPhase::Migrating
        && spec.rewrite == RewriteMode::Enabled
}

/// Rewrites every Secret, writing progress to status after each page. Failures
/// are reported by namespace/name only, never by data.
async fn run_rewrite(
    api: &Api<EtcdEncryption>,
    name: &str,
    store: &KubeSecretStore,
    status: &mut EtcdEncryptionStatus,
    is_leader: &std::sync::atomic::AtomicBool,
) -> Result<(), EtcdEncryptionReconcileError> {
    let mut progress = RewriteProgress::default();
    let mut token: Option<String> = None;
    loop {
        still_leader(is_leader)?;
        let page = rewrite_page(store, token.as_deref())
            .await
            .map_err(|err| EtcdEncryptionReconcileError::Verification(err.to_string()))?;
        progress.total += page.seen as i64;
        progress.rewritten += page.rewritten as i64;
        progress.failed += page.failed.len() as i64;
        for key in &page.failed {
            tracing::warn!(namespace = %key.namespace, secret = %key.name, "failed to rewrite secret");
        }
        status.rewrite = progress.clone();
        write_status_as_leader(api, name, status, is_leader).await?;
        match page.next {
            Some(next) => token = Some(next),
            None => return Ok(()),
        }
    }
}

pub async fn reconcile(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }
    let name = obj.name_any();
    let generation = obj.metadata.generation;
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());

    // One-time migration: a CR created by v0.1.11 carries a finalizer this
    // controller no longer handles; leaving it would hang a pending deletion.
    if let Some(remaining) = strip_finalizer(obj.metadata.finalizers.as_deref().unwrap_or(&[])) {
        tracing::info!(installation = %name, "removing the v0.1.11 finalizer");
        let patch = serde_json::json!({ "metadata": { "finalizers": remaining } });
        api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
            .await
            .map_err(EtcdEncryptionReconcileError::Api)?;
    }
    // A CR being deleted must not trigger probes, a status write or a rewrite.
    if being_deleted(&obj) {
        return Ok(Action::await_change());
    }

    let mut status = obj.status.clone().unwrap_or_default();
    status.observed_generation = generation.unwrap_or(0);

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(installation = %name, error = %err, "validation failed");
        reset_unverified(&mut status, generation, err.reason(), &err.to_string());
        write_status_as_leader(&api, &name, &status, &ctx.is_leader).await?;
        return Err(EtcdEncryptionReconcileError::Validation(err));
    }

    let target = target_prefix(&obj.spec.kms_provider_name);
    let probe = match KubeApiserverProbe::new(ctx.client.clone()) {
        Ok(probe) => probe,
        Err(err) => {
            let message = err.to_string();
            reset_unverified(&mut status, generation, "VerificationFailed", &message);
            write_status_as_leader(&api, &name, &status, &ctx.is_leader).await?;
            return Err(EtcdEncryptionReconcileError::Verification(message));
        }
    };

    // Verify every control-plane apiserver. A discovery error is an error, never "no nodes".
    let evidence = match crate::encryption_verify::verify_cluster(&probe, &target).await {
        Ok(evidence) => evidence,
        Err(err) => {
            reset_unverified(
                &mut status,
                generation,
                "VerificationFailed",
                &format!("cannot discover or verify the control-plane apiservers: {err}"),
            );
            write_status_as_leader(&api, &name, &status, &ctx.is_leader).await?;
            return Ok(Action::requeue(requeue_for(EncryptionPhase::Observing)));
        }
    };

    // The canary round trip is only worth doing when everything else is clean.
    let canary_ok = if all_clean(&evidence, &target) && obj.spec.acknowledgements.legacy_providers_removed {
        probe.canary_round_trip().await.unwrap_or_else(|err| {
            tracing::warn!(error = %err, "canary round trip failed");
            false
        })
    } else {
        false
    };

    let derivation = derive(&obj.spec, &evidence, canary_ok);
    tracing::info!(installation = %name, phase = ?derivation.phase, reason = %derivation.reason, "derived phase");

    status.phase = derivation.phase;
    status.legacy_prefixes = derivation.legacy_prefixes.clone();
    status.nodes = derivation.nodes.clone();
    status.conditions = vec![ready_condition_for(&derivation, generation)];
    write_status_as_leader(&api, &name, &status, &ctx.is_leader).await?;

    if should_run_rewrite(&obj.spec, &derivation) {
        let store = KubeSecretStore::new(ctx.client.clone());
        run_rewrite(&api, &name, &store, &mut status, &ctx.is_leader).await?;
    }

    // The canary is a probe, not state; do not leave it behind once settled.
    if matches!(
        derivation.phase,
        EncryptionPhase::ReadyToRemoveLegacy | EncryptionPhase::Verified
    ) {
        delete_canary(&ctx.client).await;
    }

    Ok(Action::requeue(requeue_for(derivation.phase)))
}

pub fn error_policy(
    _obj: Arc<EtcdEncryption>,
    _err: &EtcdEncryptionReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::{Acknowledgements, RewriteMode};

    fn spec() -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind: PlatformKind::TalosLinux,
            kms_provider_name: "barbican".to_string(),
            rewrite: RewriteMode::Disabled,
            acknowledgements: Acknowledgements::default(),
        }
    }

    #[test]
    fn accepts_talos_linux_named_default() {
        assert!(validate("default", &spec()).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let err = validate("second", &spec()).unwrap_err();

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec();
        spec.kms_provider_name = String::new();

        assert_eq!(validate("default", &spec).unwrap_err().reason(), "InvalidSpec");
    }

    #[test]
    fn strip_finalizer_removes_only_the_v0_1_11_finalizer() {
        // Review Focus 5.
        let finalizers = vec![
            "platform.rye.ninja/cleanup".to_string(),
            "other.example/keep".to_string(),
        ];

        assert_eq!(strip_finalizer(&finalizers), Some(vec!["other.example/keep".to_string()]));
    }

    #[test]
    fn strip_finalizer_is_none_when_ours_is_absent() {
        assert_eq!(strip_finalizer(&["other.example/keep".to_string()]), None);
        assert_eq!(strip_finalizer(&[]), None);
    }

    #[test]
    fn status_writes_require_current_leadership() {
        // Final review minor: leadership is re-checked before every status write.
        let leader = std::sync::atomic::AtomicBool::new(true);
        assert!(still_leader(&leader).is_ok());

        leader.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(still_leader(&leader), Err(EtcdEncryptionReconcileError::NotLeader)));
    }

    #[test]
    fn a_cr_being_deleted_stops_before_any_verification() {
        let mut obj = EtcdEncryption::new("default", spec());
        assert!(!being_deleted(&obj));

        obj.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));
        assert!(being_deleted(&obj));
    }

    #[test]
    fn condition_is_true_only_when_requested() {
        let ok = condition("Ready", true, "Verified", "done", Some(3));
        let not_ok = condition("Ready", false, "Observing", "waiting", Some(3));

        assert_eq!((ok.status.as_str(), ok.observed_generation), ("True", Some(3)));
        assert_eq!(not_ok.status, "False");
    }

    #[test]
    fn failure_reasons_are_reported_only_where_status_may_be_overwritten() {
        assert_eq!(EtcdEncryptionReconcileError::NotLeader.failure_reason(), None);
        assert_eq!(
            EtcdEncryptionReconcileError::Validation(ValidationError::UnsupportedName("x".to_string())).failure_reason(),
            None,
            "validation already wrote its own status"
        );
        assert_eq!(
            EtcdEncryptionReconcileError::Verification("x".to_string()).failure_reason(),
            Some("VerificationFailed")
        );
    }

    use crate::encryption_verdict::Derivation;

    fn derivation(phase: EncryptionPhase) -> Derivation {
        Derivation {
            phase,
            run_rewrite: false,
            reason: "why".to_string(),
            nodes: vec![],
            legacy_prefixes: vec![],
        }
    }

    #[test]
    fn reset_unverified_clears_every_trace_of_a_safe_phase() {
        for phase in [EncryptionPhase::Verified, EncryptionPhase::ReadyToRemoveLegacy] {
            let mut status = EtcdEncryptionStatus {
                phase,
                legacy_prefixes: vec!["k8s:enc:secretbox:v1:".to_string()],
                nodes: vec![Default::default()],
                rewrite: RewriteProgress { total: 5, rewritten: 4, failed: 1 },
                ..Default::default()
            };

            reset_unverified(&mut status, Some(7), "VerificationFailed", "boom");

            assert_eq!(status.phase, EncryptionPhase::Observing);
            assert!(status.nodes.is_empty() && status.legacy_prefixes.is_empty());
            assert_eq!(status.rewrite, RewriteProgress::default());
            assert_eq!(status.observed_generation, 7);
            assert_eq!(status.conditions.len(), 1);
            let c = &status.conditions[0];
            assert_eq!((c.status.as_str(), c.reason.as_str(), c.message.as_str()), ("False", "VerificationFailed", "boom"));
            assert_eq!(c.observed_generation, Some(7));
        }
    }

    #[test]
    fn active_phases_requeue_quickly_and_settled_ones_slowly() {
        use EncryptionPhase::*;
        assert_eq!(requeue_for(Observing), Duration::from_secs(30));
        assert_eq!(requeue_for(Migrating), Duration::from_secs(30));
        assert_eq!(requeue_for(NotConfigured), Duration::from_secs(600));
        assert_eq!(requeue_for(ReadyToRemoveLegacy), Duration::from_secs(600));
        assert_eq!(requeue_for(Verified), Duration::from_secs(600));
    }

    #[test]
    fn ready_is_true_only_for_verified() {
        for (phase, ready) in [
            (EncryptionPhase::Observing, false),
            (EncryptionPhase::NotConfigured, false),
            (EncryptionPhase::Migrating, false),
            (EncryptionPhase::ReadyToRemoveLegacy, false),
            (EncryptionPhase::Verified, true),
        ] {
            let c = ready_condition_for(&derivation(phase), Some(2));

            assert_eq!(c.status == "True", ready, "{phase:?}");
            assert_eq!(c.reason, format!("{phase:?}"));
            assert_eq!(c.message, "why");
        }
    }

    #[test]
    fn rewrite_only_runs_when_the_derivation_asks_for_it_and_the_spec_enables_it() {
        // Review Focus 6: belt and braces on top of derive().
        let mut spec = spec();
        let mut d = derivation(EncryptionPhase::Migrating);
        d.run_rewrite = true;

        spec.rewrite = RewriteMode::Disabled;
        assert!(!should_run_rewrite(&spec, &d));

        spec.rewrite = RewriteMode::Enabled;
        assert!(should_run_rewrite(&spec, &d));

        d.run_rewrite = false;
        assert!(!should_run_rewrite(&spec, &d));

        let mut wrong_phase = derivation(EncryptionPhase::Observing);
        wrong_phase.run_rewrite = true;
        assert!(!should_run_rewrite(&spec, &wrong_phase));
    }
}
