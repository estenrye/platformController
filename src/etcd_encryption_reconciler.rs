use crate::crd::{Condition, PlatformKind};
use crate::encryption_phase::{cleanup_step, next_phase, CleanupInputs, CleanupStep, PhaseInputs};
use crate::etcd_encryption::{
    EncryptionPhase, EtcdEncryption, EtcdEncryptionSpec, EtcdEncryptionSpecError, EtcdEncryptionStatus,
    RewriteProgress, TalosPatches,
};
use crate::kms_provider::{kms_plan, KmsPlan, PLUGIN_NAMESPACE};
use crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME};
use crate::secret_rewrite::{rewrite_page, KubeSecretStore, SecretStore, StoreError};
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::Node;
use kube::api::ListParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

const FIELD_MANAGER: &str = "platform-controller";
const WAITING_REQUEUE: Duration = Duration::from_secs(15);
const STEADY_REQUEUE: Duration = Duration::from_secs(300);

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
    #[error(transparent)]
    Apply(#[from] crate::apply::ApplyError),
    #[error("kubernetes API call failed: {0}")]
    Api(#[source] kube::Error),
    #[error("secret rewrite failed: {0}")]
    Store(#[from] StoreError),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
    #[error("not the leader; standing down")]
    NotLeader,
    /// Deletion is deliberately waiting on the operator or a probe. Always an
    /// `Err`: `kube::runtime::finalizer` strips the finalizer on any `Ok` from
    /// the Cleanup arm, and the plugin must outlive the apiserver's use of it.
    #[error("deletion is waiting: {0}")]
    CleanupBlocked(String),
}

impl EtcdEncryptionReconcileError {
    /// The `Ready=False` reason to report, or `None` when this error must not
    /// overwrite status. Exhaustive on purpose: a new variant forces a decision.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            EtcdEncryptionReconcileError::Apply(_) => Some("ApplyFailed"),
            EtcdEncryptionReconcileError::Api(_) => Some("ApiCallFailed"),
            EtcdEncryptionReconcileError::Store(_) => Some("RewriteFailed"),
            EtcdEncryptionReconcileError::Validation(_)
            | EtcdEncryptionReconcileError::Status(_)
            | EtcdEncryptionReconcileError::NotLeader
            | EtcdEncryptionReconcileError::CleanupBlocked(_) => None,
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

/// A Ready plugin pod must exist on **every** control-plane node, and there
/// must be at least one: patch 1 triggers a rolling apiserver restart that
/// needs the socket on each node it lands on.
/// The DaemonSet controller has observed the current spec and every pod is
/// on the current revision, so stale pods from a previous image generation
/// during a rollout never count as ready. Missing values mean not ready.
pub fn daemonset_rollout_current(
    generation: Option<i64>,
    observed_generation: Option<i64>,
    updated: Option<i32>,
    desired: i32,
) -> bool {
    matches!((generation, observed_generation), (Some(g), Some(o)) if g == o) && updated.unwrap_or(0) == desired
}

pub fn daemonset_covers_all_control_plane(desired: i32, ready: i32, control_plane_nodes: usize) -> bool {
    control_plane_nodes > 0 && desired >= 0 && desired as usize == control_plane_nodes && ready == desired
}

/// The patches visible once `phase` is reached: additive, never retracted.
pub fn patches_for(phase: EncryptionPhase, plan: &KmsPlan, current: &TalosPatches) -> TalosPatches {
    use EncryptionPhase::*;
    let mut patches = current.clone();
    if matches!(phase, AwaitingKmsConfig | Rewriting | AwaitingPlaintextRemoval | Encrypted) {
        patches.enable_kms = Some(crate::talos_patches::enable_kms(plan));
    }
    if matches!(phase, AwaitingPlaintextRemoval | Encrypted) {
        patches.remove_identity = Some(crate::talos_patches::remove_identity(plan));
    }
    patches
}

async fn write_status(
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

async fn plugin_ready(client: &kube::Client, plan: &KmsPlan) -> Result<bool, EtcdEncryptionReconcileError> {
    let daemonsets: Api<DaemonSet> = Api::namespaced(client.clone(), PLUGIN_NAMESPACE);
    let status = daemonsets
        .get_opt(&plan.daemonset_name)
        .await
        .map_err(EtcdEncryptionReconcileError::Api)?
        ;
    let Some(daemonset) = status else { return Ok(false) };
    let generation = daemonset.metadata.generation;
    let Some(status) = daemonset.status else { return Ok(false) };
    if !daemonset_rollout_current(
        generation,
        status.observed_generation,
        status.updated_number_scheduled,
        status.desired_number_scheduled,
    ) {
        return Ok(false);
    }
    let nodes: Api<Node> = Api::all(client.clone());
    let control_plane = nodes
        .list(&ListParams::default().labels("node-role.kubernetes.io/control-plane"))
        .await
        .map_err(EtcdEncryptionReconcileError::Api)?
        .items
        .len();
    Ok(daemonset_covers_all_control_plane(status.desired_number_scheduled, status.number_ready, control_plane))
}

/// Rewriting while KMS is not the active write provider would store Secrets
/// under identity, and a later remove-identity patch would make them
/// unreadable. Only rewrite while the apiserver reports an active KMS provider.
pub fn should_run_rewrite(phase: EncryptionPhase, kms_active: bool) -> bool {
    phase == EncryptionPhase::Rewriting && kms_active
}

/// A probe error is "not yet": it is logged and never advances a phase.
///
/// Forward path only. Mapping an error to `false` is the SAFE direction here
/// (it blocks advancing). `cleanup` must NOT use this: there `false` would
/// mean "KMS is off, the plugin may be removed", so it uses `Option<bool>`.
async fn probe_kms_active(client: &kube::Client) -> bool {
    crate::encryption_probe::kms_active(client).await.unwrap_or_else(|err| {
        tracing::warn!(error = %err, "KMS metrics probe failed; treating as not active");
        false
    })
}

/// Rewrites every Secret, writing progress to status after each page.
/// Returns true only when every Secret was rewritten with zero failures.
async fn run_rewrite(
    api: &Api<EtcdEncryption>,
    name: &str,
    store: &impl SecretStore,
    status: &mut EtcdEncryptionStatus,
) -> Result<bool, EtcdEncryptionReconcileError> {
    let mut progress = RewriteProgress::default();
    let mut token: Option<String> = None;
    loop {
        let page = rewrite_page(store, token.as_deref()).await?;
        progress.total += page.seen;
        progress.rewritten += page.rewritten;
        progress.failed += page.failed.len() as u64;
        for key in &page.failed {
            // Identity only, never data.
            tracing::warn!(namespace = %key.namespace, secret = %key.name, "failed to rewrite secret");
        }
        status.rewrite = progress.clone();
        write_status(api, name, status).await?;
        match page.next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    Ok(progress.failed == 0)
}

pub async fn reconcile(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    let mut progress = crate::ledger::ReconcileProgress::default();
    let result = reconcile_inner(obj.clone(), ctx.clone(), &mut progress).await;
    if let Err(err) = &result
        && let Some(reason) = err.failure_reason()
    {
        record_failure(&obj, &ctx, &progress, reason, &err.to_string()).await;
    }
    result
}

/// Best effort. Keeps the phase and every other field: a failure is a
/// condition, never a regression of the protocol position.
async fn record_failure(
    obj: &EtcdEncryption,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    let mut status = obj.status.clone().unwrap_or_default();
    status.applied_resources = crate::ledger::failure_ledger(&status.applied_resources, progress.desired.as_deref());
    status.conditions = vec![condition("Ready", false, reason, message, obj.metadata.generation)];
    if let Err(err) = write_status(&api, &name, &status).await {
        tracing::warn!(installation = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, EtcdEncryptionReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }
    let name = obj.name_any();
    let generation = obj.metadata.generation;
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    let mut status = obj.status.clone().unwrap_or_default();
    status.observed_generation = generation.unwrap_or(0);

    if let Err(err) = validate(&name, &obj.spec) {
        tracing::warn!(installation = %name, error = %err, "validation failed");
        status.conditions = vec![condition("Ready", false, err.reason(), &err.to_string(), generation)];
        write_status(&api, &name, &status).await?;
        return Err(EtcdEncryptionReconcileError::Validation(err));
    }
    let plan = kms_plan(&obj.spec).map_err(ValidationError::Spec)?;

    // Checkpoint the ledger before the first apply, like every other component.
    let desired = vec![crate::apply::resource_ref(&plan.daemonset)];
    progress.desired = Some(desired.clone());
    if let Some(ledger) = crate::ledger::checkpoint_ledger(&status.applied_resources, &desired) {
        status.applied_resources = ledger;
        status.conditions = vec![condition("Ready", false, "Applying", "applying the KMS plugin", generation)];
        write_status(&api, &name, &status).await?;
    }
    crate::apply::apply_object(&ctx.client, &plan.daemonset, FIELD_MANAGER).await?;

    // Gather the facts. Probes only run once they can matter.
    let mut inputs = PhaseInputs {
        current: status.phase,
        acks: obj.spec.acknowledgements,
        plugin_ready: plugin_ready(&ctx.client, &plan).await?,
        kms_active: false,
        canary_ok: false,
        rewrite_complete: false,
    };
    if !matches!(inputs.current, EncryptionPhase::Pending | EncryptionPhase::InstallingPlugin) {
        // Asymmetry with `cleanup`: here a failed probe is `false` (blocks
        // advancing); in cleanup it is `None` (blocks plugin removal).
        inputs.kms_active = probe_kms_active(&ctx.client).await;
    }
    if matches!(inputs.current, EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted) {
        inputs.canary_ok = crate::encryption_probe::canary_round_trips(&ctx.client)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "canary probe failed; treating as not round-tripping");
                false
            });
    }

    // Walk forward as far as the facts allow in one reconcile.
    let store = KubeSecretStore::new(ctx.client.clone());
    loop {
        if should_run_rewrite(inputs.current, inputs.kms_active) {
            inputs.rewrite_complete = run_rewrite(&api, &name, &store, &mut status).await?;
        }
        let next = next_phase(&inputs);
        if next == inputs.current {
            break;
        }
        tracing::info!(installation = %name, from = ?inputs.current, to = ?next, "phase transition");
        inputs.current = next;
        if matches!(next, EncryptionPhase::Rewriting | EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted)
            && !inputs.kms_active
        {
            inputs.kms_active = probe_kms_active(&ctx.client).await;
        }
        if matches!(next, EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted) && !inputs.canary_ok {
            inputs.canary_ok = crate::encryption_probe::canary_round_trips(&ctx.client)
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "canary probe failed; treating as not round-tripping");
                    false
                });
        }
    }

    status.phase = inputs.current;
    status.talos_patches = patches_for(status.phase, &plan, &status.talos_patches);
    let (ready, degraded) = match status.phase {
        EncryptionPhase::Encrypted => (true, !(inputs.kms_active && inputs.canary_ok && inputs.plugin_ready)),
        _ => (false, false),
    };
    let message = if status.phase == EncryptionPhase::Rewriting && !inputs.kms_active {
        "waiting for the apiserver to report an active KMS provider before rewriting Secrets".to_string()
    } else {
        phase_message(status.phase)
    };
    status.conditions = vec![condition("Ready", ready && !degraded, &format!("{:?}", status.phase), &message, generation)];
    if degraded {
        status.conditions.push(condition(
            "Degraded",
            true,
            "ProbeNegative",
            "a probe that passed earlier is now failing; the phase is not regressed. Check the plugin pods and the apiserver",
            generation,
        ));
    }
    write_status(&api, &name, &status).await?;

    Ok(Action::requeue(if status.phase == EncryptionPhase::Encrypted { STEADY_REQUEUE } else { WAITING_REQUEUE }))
}

fn phase_message(phase: EncryptionPhase) -> String {
    use EncryptionPhase::*;
    match phase {
        Pending | InstallingPlugin => "installing the KMS plugin on every control-plane node",
        AwaitingKmsConfig => "apply status.talosPatches.enableKms with talosctl, then set spec.acknowledgements.kmsConfigApplied",
        Rewriting => "re-encrypting every Secret through KMS",
        AwaitingPlaintextRemoval => "apply status.talosPatches.removeIdentity with talosctl, then set spec.acknowledgements.plaintextRemoved",
        Encrypted => "Secrets are encrypted at rest through KMS; verified by apiserver probes",
        RevertingKms | Decrypting | AwaitingKmsRemoval => "reverting encryption before deletion",
    }
    .to_string()
}

pub fn error_policy(
    _obj: Arc<EtcdEncryption>,
    _err: &kube::runtime::finalizer::Error<EtcdEncryptionReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

/// Whether this cleanup step may return `Ok`. `kube::runtime::finalizer`
/// strips the finalizer on any `Ok` from the Cleanup arm, so only
/// `RemovePlugin` (the last step) may finish; every other step must `Err`.
pub fn cleanup_may_finish(step: CleanupStep) -> bool {
    match step {
        CleanupStep::RemovePlugin => true,
        CleanupStep::AwaitRevertAck | CleanupStep::Decrypt | CleanupStep::AwaitKmsRemoval => false,
    }
}

pub async fn cleanup(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    // Never return Ok from a standby's Cleanup dispatch; see `reconciler::cleanup`.
    if !ctx.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(EtcdEncryptionReconcileError::NotLeader);
    }
    let name = obj.name_any();
    let generation = obj.metadata.generation;
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
    let mut status = obj.status.clone().unwrap_or_default();
    let plan = kms_plan(&obj.spec).map_err(ValidationError::Spec)?;
    // Asymmetry with the forward path: a failed probe is `None` (unknown),
    // never `false`. `false` would mean "KMS is off" and permit removing the
    // plugin while the apiserver may still depend on it.
    let kms_active = match crate::encryption_probe::kms_active(&ctx.client).await {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::warn!(error = %err, "KMS metrics probe failed; treating as unknown");
            None
        }
    };

    let step = cleanup_step(&CleanupInputs { phase: status.phase, acks: obj.spec.acknowledgements, kms_active });
    tracing::info!(installation = %name, phase = ?status.phase, ?step, "cleanup step");

    let result: Result<Action, EtcdEncryptionReconcileError> = async { match step {
        CleanupStep::RemovePlugin => {
            for reference in status.applied_resources.iter().rev() {
                tracing::info!(kind = %reference.kind, resource = %reference.name, "deleting applied resource");
                crate::apply::delete_object(&ctx.client, reference).await?;
            }
            crate::encryption_probe::delete_canary(&ctx.client).await;
            Ok(Action::await_change())
        }
        CleanupStep::AwaitRevertAck => {
            status.phase = EncryptionPhase::RevertingKms;
            status.talos_patches.revert = Some(crate::talos_patches::revert(&plan));
            status.conditions = vec![condition("Ready", false, "RevertingKms", "apply status.talosPatches.revert with talosctl, then set spec.acknowledgements.kmsReverted", generation)];
            write_status(&api, &name, &status).await?;
            Err(EtcdEncryptionReconcileError::CleanupBlocked("waiting for spec.acknowledgements.kmsReverted".to_string()))
        }
        CleanupStep::Decrypt => {
            status.phase = EncryptionPhase::Decrypting;
            write_status(&api, &name, &status).await?;
            let store = KubeSecretStore::new(ctx.client.clone());
            if run_rewrite(&api, &name, &store, &mut status).await? {
                status.phase = EncryptionPhase::AwaitingKmsRemoval;
                status.talos_patches.remove_kms = Some(crate::talos_patches::remove_kms());
                status.conditions = vec![condition("Ready", false, "AwaitingKmsRemoval", "apply status.talosPatches.removeKms with talosctl, then set spec.acknowledgements.kmsRemoved", generation)];
                write_status(&api, &name, &status).await?;
                Err(EtcdEncryptionReconcileError::CleanupBlocked("waiting for spec.acknowledgements.kmsRemoved".to_string()))
            } else {
                Err(EtcdEncryptionReconcileError::CleanupBlocked("some Secrets could not be rewritten; retrying".to_string()))
            }
        }
        CleanupStep::AwaitKmsRemoval => Err(EtcdEncryptionReconcileError::CleanupBlocked(
            "waiting for spec.acknowledgements.kmsRemoved and for the apiserver to stop reporting a KMS provider".to_string(),
        )),
    } }
    .await;
    enforce_cleanup_gate(step, result)
}

/// Last line of defence: `kube::runtime::finalizer` strips the finalizer on
/// any `Ok` from Cleanup, so an `Ok` from any step other than the final one
/// is converted into a blocking `Err`. Errors pass through unchanged.
pub fn enforce_cleanup_gate(
    step: CleanupStep,
    result: Result<Action, EtcdEncryptionReconcileError>,
) -> Result<Action, EtcdEncryptionReconcileError> {
    match result {
        Ok(_) if !cleanup_may_finish(step) => Err(EtcdEncryptionReconcileError::CleanupBlocked(
            "internal: cleanup tried to finish before the plugin was safe to remove".to_string(),
        )),
        other => other,
    }
}

pub async fn reconcile_with_finalizer(
    obj: Arc<EtcdEncryption>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<EtcdEncryptionReconcileError>> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }
    let api: Api<EtcdEncryption> = Api::all(ctx.client.clone());
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
    use crate::etcd_encryption::{BarbicanSpec, KmsProviderKind, SecretNameRef};

    fn spec_with(platform_kind: PlatformKind) -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind,
            provider: KmsProviderKind::Barbican,
            barbican: Some(BarbicanSpec {
                image: "img:1".to_string(),
                cloud_config_secret_ref: SecretNameRef { name: "cc".to_string() },
            }),
            acknowledgements: Default::default(),
        }
    }

    #[test]
    fn accepts_talos_linux_named_default() {
        assert!(validate("default", &spec_with(PlatformKind::TalosLinux)).is_ok());
    }

    #[test]
    fn rejects_installations_not_named_default() {
        let err = validate("second", &spec_with(PlatformKind::TalosLinux)).unwrap_err();

        assert!(matches!(&err, ValidationError::UnsupportedName(name) if name == "second"));
        assert_eq!(err.reason(), "Unsupported");
    }

    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux);
        spec.acknowledgements.plaintext_removed = true;

        assert_eq!(validate("default", &spec).unwrap_err().reason(), "InvalidAcknowledgements");
    }

    #[test]
    fn plugin_is_ready_only_when_it_covers_every_control_plane_node() {
        // Review Focus 3.
        assert!(daemonset_covers_all_control_plane(3, 3, 3));
        assert!(!daemonset_covers_all_control_plane(2, 2, 3), "scheduled on fewer nodes than exist");
        assert!(!daemonset_covers_all_control_plane(3, 2, 3), "a pod is not Ready yet");
        assert!(!daemonset_covers_all_control_plane(0, 0, 0), "no control-plane nodes visible");
        assert!(!daemonset_covers_all_control_plane(0, 0, 3), "nothing scheduled yet");
    }

    #[test]
    fn failure_reasons_cover_every_reportable_variant_and_skip_the_rest() {
        let apply = EtcdEncryptionReconcileError::Apply(crate::apply::ApplyError::KindNotAvailable {
            api_version: "apps/v1".to_string(),
            kind: "DaemonSet".to_string(),
            timeout: std::time::Duration::from_secs(1),
            detail: "x".to_string(),
        });
        assert_eq!(apply.failure_reason(), Some("ApplyFailed"));
        assert_eq!(
            EtcdEncryptionReconcileError::Store(crate::secret_rewrite::StoreError("x".to_string())).failure_reason(),
            Some("RewriteFailed")
        );
        assert_eq!(EtcdEncryptionReconcileError::NotLeader.failure_reason(), None);
        assert_eq!(
            EtcdEncryptionReconcileError::CleanupBlocked("x".to_string()).failure_reason(),
            None,
            "a blocked cleanup is a wait, not a failure to report"
        );
    }

    #[test]
    fn condition_is_ready_true_only_when_requested() {
        let ok = condition("Ready", true, "Encrypted", "done", Some(3));
        let not_ok = condition("Ready", false, "Rewriting", "in progress", Some(3));

        assert_eq!((ok.status.as_str(), ok.observed_generation), ("True", Some(3)));
        assert_eq!(not_ok.status, "False");
    }

    #[test]
    fn only_remove_plugin_may_finish_cleanup() {
        // kube's finalizer strips the finalizer on any Ok from Cleanup.
        assert!(cleanup_may_finish(CleanupStep::RemovePlugin));
        assert!(!cleanup_may_finish(CleanupStep::AwaitRevertAck));
        assert!(!cleanup_may_finish(CleanupStep::Decrypt));
        assert!(!cleanup_may_finish(CleanupStep::AwaitKmsRemoval));
    }

    #[test]
    fn patches_for_a_phase_include_everything_published_so_far() {
        let plan = crate::kms_provider::kms_plan(&spec_with(PlatformKind::TalosLinux)).unwrap();

        let early = patches_for(EncryptionPhase::AwaitingKmsConfig, &plan, &TalosPatches::default());
        assert!(early.enable_kms.is_some() && early.remove_identity.is_none());

        let later = patches_for(EncryptionPhase::AwaitingPlaintextRemoval, &plan, &early);
        assert!(later.enable_kms.is_some() && later.remove_identity.is_some());

        let before = patches_for(EncryptionPhase::InstallingPlugin, &plan, &TalosPatches::default());
        assert!(before.enable_kms.is_none(), "no patch before the plugin is ready everywhere");
    }

    #[test]
    fn cleanup_gate_turns_premature_ok_into_blocked_err() {
        for step in [CleanupStep::AwaitRevertAck, CleanupStep::Decrypt, CleanupStep::AwaitKmsRemoval] {
            let gated = enforce_cleanup_gate(step, Ok(Action::await_change()));
            assert!(matches!(gated, Err(EtcdEncryptionReconcileError::CleanupBlocked(_))), "{step:?}");
        }
        assert!(enforce_cleanup_gate(CleanupStep::RemovePlugin, Ok(Action::await_change())).is_ok());
        let err = enforce_cleanup_gate(
            CleanupStep::Decrypt,
            Err(EtcdEncryptionReconcileError::NotLeader),
        );
        assert!(matches!(err, Err(EtcdEncryptionReconcileError::NotLeader)));
    }

    #[test]
    fn rewrite_runs_only_in_rewriting_with_kms_active() {
        assert!(should_run_rewrite(EncryptionPhase::Rewriting, true));
        assert!(!should_run_rewrite(EncryptionPhase::Rewriting, false));
        assert!(!should_run_rewrite(EncryptionPhase::AwaitingKmsConfig, true));
        assert!(!should_run_rewrite(EncryptionPhase::Encrypted, true));
    }

    #[test]
    fn daemonset_rollout_must_be_current() {
        assert!(daemonset_rollout_current(Some(2), Some(2), Some(3), 3));
        assert!(!daemonset_rollout_current(Some(2), Some(1), Some(3), 3), "controller has not observed the new spec");
        assert!(!daemonset_rollout_current(Some(2), Some(2), Some(1), 3), "stale pods from the old revision");
        assert!(!daemonset_rollout_current(Some(2), Some(2), None, 3));
        assert!(!daemonset_rollout_current(None, Some(2), Some(3), 3));
        assert!(!daemonset_rollout_current(Some(2), None, Some(3), 3));
    }

    #[test]
    fn reconcile_futures_are_send() {
        // kube's Controller::run requires Send futures; compile-time check only.
        fn assert_send<T: Send>(_: &T) {}
        #[allow(dead_code)]
        fn check(obj: Arc<EtcdEncryption>, ctx: Arc<Context>) {
            assert_send(&reconcile_with_finalizer(obj, ctx));
        }
    }
}
