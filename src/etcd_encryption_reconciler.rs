use crate::crd::{Condition, PlatformKind};
use crate::encryption_phase::{cleanup_step, next_phase, CleanupInputs, CleanupStep, PhaseInputs};
use crate::etcd_encryption::{
    effective_acks, Acknowledgements, EncryptionPhase, EtcdEncryption, EtcdEncryptionSpec, EtcdEncryptionSpecError,
    EtcdEncryptionStatus, PatchGenerations, RewriteProgress, TalosPatches,
};
use crate::kms_provider::{kms_plan, KmsPlan, PLUGIN_NAMESPACE};
use crate::reconciler::{leader_gate, Context, FINALIZER_NAME, SINGLETON_NAME};
use crate::secret_rewrite::{rewrite_page, verify_listable, KubeSecretStore, SecretStore, StoreError};
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
/// Upper bound on listing every Secret for the readable gate.
const LISTABLE_TIMEOUT: Duration = Duration::from_secs(120);

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

/// Records `generation` for every patch in `patches` that is published but has
/// no publication generation yet. Never overwrites one already recorded.
/// Call it on the status that the publishing write sends, so the patch and its
/// generation land in the same write. Returns whether anything changed.
pub fn record_publication(gens: &mut PatchGenerations, patches: &TalosPatches, generation: i64) -> bool {
    let mut changed = false;
    let mut record = |published: &mut Option<i64>, patch: &Option<String>| {
        if patch.is_some() && published.is_none() {
            *published = Some(generation);
            changed = true;
        }
    };
    record(&mut gens.enable_kms, &patches.enable_kms);
    record(&mut gens.remove_identity, &patches.remove_identity);
    record(&mut gens.revert, &patches.revert);
    record(&mut gens.remove_kms, &patches.remove_kms);
    changed
}

/// The cleanup state machine's inputs. Acks count only after their patch was
/// published (`effective_acks`), except `kms_config_applied`, which stays the
/// RAW spec value: it only decides engaged-ness, and there a not-yet-counting
/// ack must still keep the plugin.
pub fn cleanup_inputs(
    phase: EncryptionPhase,
    raw: Acknowledgements,
    generation: i64,
    gens: &PatchGenerations,
    kms_active: Option<bool>,
    secrets_readable: bool,
) -> CleanupInputs {
    let acks = Acknowledgements { kms_config_applied: raw.kms_config_applied, ..effective_acks(raw, generation, gens) };
    CleanupInputs { phase, acks, kms_active, secrets_readable }
}

/// The phase to record while waiting for `kmsReverted`. The phase never
/// regresses: once decrypting has started (or finished), an ack that does not
/// count (flipped, or written before `patchGenerations` existed) keeps the
/// phase and only blocks the next step.
pub fn revert_wait_phase(current: EncryptionPhase) -> EncryptionPhase {
    match current {
        EncryptionPhase::Decrypting | EncryptionPhase::AwaitingKmsRemoval => current,
        _ => EncryptionPhase::RevertingKms,
    }
}

/// Every Secret can be listed (so decrypted) right now. Errors and timeouts
/// are `false` and logged: this gate only ever blocks.
async fn probe_secrets_readable(store: &impl SecretStore) -> bool {
    let deadline = tokio::time::Instant::now() + LISTABLE_TIMEOUT;
    match tokio::time::timeout_at(deadline, verify_listable(store)).await {
        Ok(Ok(_)) => true,
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "listing every Secret failed; treating Secrets as not all readable");
            false
        }
        Err(_) => {
            tracing::warn!("listing every Secret timed out; treating Secrets as not all readable");
            false
        }
    }
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
    // An ack counts only if it was set after its patch was published.
    let mut inputs = PhaseInputs {
        current: status.phase,
        acks: effective_acks(obj.spec.acknowledgements, generation.unwrap_or(0), &status.patch_generations),
        plugin_ready: plugin_ready(&ctx.client, &plan).await?,
        kms_active: false,
        canary_ok: false,
        rewrite_complete: false,
        secrets_readable: false,
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
    let store = KubeSecretStore::new(ctx.client.clone());
    if matches!(inputs.current, EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted) {
        inputs.secrets_readable = probe_secrets_readable(&store).await;
    }

    // Walk forward as far as the facts allow in one reconcile.
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
        if matches!(next, EncryptionPhase::AwaitingPlaintextRemoval | EncryptionPhase::Encrypted)
            && !inputs.secrets_readable
        {
            inputs.secrets_readable = probe_secrets_readable(&store).await;
        }
    }

    status.phase = inputs.current;
    status.talos_patches = patches_for(status.phase, &plan, &status.talos_patches);
    // Same write as the publication: an ack already set now does not count.
    record_publication(&mut status.patch_generations, &status.talos_patches, generation.unwrap_or(0));
    let (ready, degraded) = match status.phase {
        EncryptionPhase::Encrypted => (
            true,
            !(inputs.kms_active && inputs.canary_ok && inputs.plugin_ready && inputs.secrets_readable),
        ),
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
        Encrypted => {
            "Secrets are encrypted at rest through KMS. Established indirectly: the apiserver reports a KMS \
             provider, a canary Secret round-trips, every Secret can be read, and plaintext removal was \
             acknowledged; etcd itself cannot be read through the Kubernetes API."
        }
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

    let store = KubeSecretStore::new(ctx.client.clone());
    // Listing every Secret only matters for the final step.
    let secrets_readable =
        status.phase == EncryptionPhase::AwaitingKmsRemoval && probe_secrets_readable(&store).await;
    let current_generation = generation.unwrap_or(0);
    let inputs = cleanup_inputs(status.phase, obj.spec.acknowledgements, current_generation, &status.patch_generations, kms_active, secrets_readable);
    let step = cleanup_step(&inputs);
    tracing::info!(installation = %name, phase = ?status.phase, ?step, "cleanup step");

    let result: Result<Action, EtcdEncryptionReconcileError> = async {
        if step != CleanupStep::RemovePlugin {
            // The apiserver may still need the plugin: keep it applied (a
            // drifted or deleted DaemonSet is restored) until the last step.
            crate::apply::apply_object(&ctx.client, &plan.daemonset, FIELD_MANAGER).await?;
        }
        match step {
        CleanupStep::RemovePlugin => {
            for reference in status.applied_resources.iter().rev() {
                tracing::info!(kind = %reference.kind, resource = %reference.name, "deleting applied resource");
                crate::apply::delete_object(&ctx.client, reference).await?;
            }
            crate::encryption_probe::delete_canary(&ctx.client).await;
            Ok(Action::await_change())
        }
        CleanupStep::AwaitRevertAck => {
            status.phase = revert_wait_phase(status.phase);
            status.talos_patches.revert = Some(crate::talos_patches::revert(&plan));
            record_publication(&mut status.patch_generations, &status.talos_patches, current_generation);
            status.conditions = vec![condition("Ready", false, "RevertingKms", "apply status.talosPatches.revert with talosctl, then set spec.acknowledgements.kmsReverted (flip it false then true if it was already set when the patch appeared)", generation)];
            write_status(&api, &name, &status).await?;
            Err(EtcdEncryptionReconcileError::CleanupBlocked("waiting for spec.acknowledgements.kmsReverted, set after status.talosPatches.revert was published".to_string()))
        }
        CleanupStep::Decrypt => {
            if !plugin_ready(&ctx.client, &plan).await? {
                return Err(EtcdEncryptionReconcileError::CleanupBlocked(
                    "waiting for the KMS plugin to be Ready on every control-plane node before decrypting".to_string(),
                ));
            }
            status.phase = EncryptionPhase::Decrypting;
            record_publication(&mut status.patch_generations, &status.talos_patches, current_generation);
            write_status(&api, &name, &status).await?;
            if run_rewrite(&api, &name, &store, &mut status).await? {
                status.phase = EncryptionPhase::AwaitingKmsRemoval;
                status.talos_patches.remove_kms = Some(crate::talos_patches::remove_kms());
                record_publication(&mut status.patch_generations, &status.talos_patches, current_generation);
                status.conditions = vec![condition("Ready", false, "AwaitingKmsRemoval", "apply status.talosPatches.removeKms with talosctl, then set spec.acknowledgements.kmsRemoved (flip it false then true if it was already set when the patch appeared)", generation)];
                write_status(&api, &name, &status).await?;
                Err(EtcdEncryptionReconcileError::CleanupBlocked("waiting for spec.acknowledgements.kmsRemoved".to_string()))
            } else {
                Err(EtcdEncryptionReconcileError::CleanupBlocked("some Secrets could not be rewritten; retrying".to_string()))
            }
        }
        CleanupStep::AwaitKmsRemoval => {
            // A status written before patchGenerations existed: record the
            // already-published patches now so their acks can start counting.
            if record_publication(&mut status.patch_generations, &status.talos_patches, current_generation) {
                write_status(&api, &name, &status).await?;
            }
            Err(EtcdEncryptionReconcileError::CleanupBlocked(
                "waiting for spec.acknowledgements.kmsRemoved (set after status.talosPatches.removeKms was published), \
                 for the apiserver to stop reporting a KMS provider, and for every Secret to be readable"
                    .to_string(),
            ))
        }
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
    use crate::etcd_encryption::{Acknowledgements, BarbicanSpec, KmsProviderKind, SecretNameRef};

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

    const ALL_ACKS: Acknowledgements =
        Acknowledgements { kms_config_applied: true, plaintext_removed: true, kms_reverted: true, kms_removed: true };

    #[test]
    fn a_preset_kms_reverted_does_not_skip_publishing_the_revert_patch() {
        // C1: the operator set kmsReverted before deleting. The revert patch
        // was never published, so the ack cannot be for it: run the revert
        // step (publish the patch), never Decrypt.
        let raw = Acknowledgements { kms_config_applied: true, kms_reverted: true, ..Default::default() };
        let gens = PatchGenerations { enable_kms: Some(2), remove_identity: Some(3), ..Default::default() };

        let inputs = cleanup_inputs(EncryptionPhase::Encrypted, raw, 7, &gens, Some(true), true);

        assert!(!inputs.acks.kms_reverted);
        assert_eq!(cleanup_step(&inputs), CleanupStep::AwaitRevertAck);
    }

    #[test]
    fn cleanup_engagement_uses_the_raw_kms_config_applied() {
        // Engaged-ness errs on the safe side: an ack that does not count yet
        // still means the operator may have applied patch 1.
        let raw = Acknowledgements { kms_config_applied: true, ..Default::default() };

        let inputs = cleanup_inputs(EncryptionPhase::AwaitingKmsConfig, raw, 5, &PatchGenerations::default(), Some(false), true);

        assert!(inputs.acks.kms_config_applied);
        assert_eq!(cleanup_step(&inputs), CleanupStep::AwaitRevertAck);
    }

    #[test]
    fn cleanup_inputs_count_acks_given_after_their_patches() {
        let gens = PatchGenerations { enable_kms: Some(2), remove_identity: Some(3), revert: Some(4), remove_kms: Some(5) };

        let inputs = cleanup_inputs(EncryptionPhase::AwaitingKmsRemoval, ALL_ACKS, 6, &gens, Some(false), true);

        assert_eq!(inputs.acks, ALL_ACKS);
        assert_eq!(cleanup_step(&inputs), CleanupStep::RemovePlugin);
        let unreadable = cleanup_inputs(EncryptionPhase::AwaitingKmsRemoval, ALL_ACKS, 6, &gens, Some(false), false);
        assert_eq!(cleanup_step(&unreadable), CleanupStep::AwaitKmsRemoval);
    }

    #[test]
    fn publication_records_the_generation_of_each_newly_published_patch_once() {
        let mut gens = PatchGenerations::default();
        let patches = TalosPatches { enable_kms: Some("p1".to_string()), ..Default::default() };

        assert!(record_publication(&mut gens, &patches, 3));
        assert_eq!(gens, PatchGenerations { enable_kms: Some(3), ..Default::default() });

        let more = TalosPatches { remove_identity: Some("p2".to_string()), ..patches };
        assert!(record_publication(&mut gens, &more, 5));
        assert_eq!(gens.enable_kms, Some(3), "never overwritten");
        assert_eq!(gens.remove_identity, Some(5));

        assert!(!record_publication(&mut gens, &more, 9), "nothing new to record");
        assert_eq!(gens, PatchGenerations { enable_kms: Some(3), remove_identity: Some(5), ..Default::default() });
    }

    #[test]
    fn a_preset_plaintext_removed_does_not_walk_to_encrypted() {
        // I4: plaintextRemoved was already true when the remove-identity patch
        // was first published (same generation): it does not count.
        let plan = crate::kms_provider::kms_plan(&spec_with(PlatformKind::TalosLinux)).unwrap();
        let generation = 4;
        let mut gens = PatchGenerations { enable_kms: Some(2), ..Default::default() };
        let patches = patches_for(EncryptionPhase::AwaitingPlaintextRemoval, &plan, &TalosPatches::default());
        record_publication(&mut gens, &patches, generation);

        let raw = Acknowledgements { kms_config_applied: true, plaintext_removed: true, ..Default::default() };
        let inputs = PhaseInputs {
            current: EncryptionPhase::AwaitingPlaintextRemoval,
            acks: effective_acks(raw, generation, &gens),
            plugin_ready: true,
            kms_active: true,
            canary_ok: true,
            rewrite_complete: true,
            secrets_readable: true,
        };
        assert_eq!(next_phase(&inputs), EncryptionPhase::AwaitingPlaintextRemoval);

        // After the operator flips it (generation 6), it counts.
        let later = PhaseInputs { acks: effective_acks(raw, 6, &gens), ..inputs };
        assert_eq!(next_phase(&later), EncryptionPhase::Encrypted);
    }

    #[test]
    fn the_encrypted_message_says_how_encryption_was_established() {
        let message = phase_message(EncryptionPhase::Encrypted);

        assert!(message.starts_with("Secrets are encrypted at rest through KMS. Established indirectly:"));
        assert!(message.contains("every Secret can be read"));
        assert!(message.contains("etcd itself cannot be read through the Kubernetes API"));
    }

    #[test]
    fn waiting_for_the_revert_ack_never_regresses_a_deletion_phase() {
        use EncryptionPhase::*;
        for phase in [Pending, InstallingPlugin, AwaitingKmsConfig, Rewriting, AwaitingPlaintextRemoval, Encrypted, RevertingKms] {
            assert_eq!(revert_wait_phase(phase), RevertingKms, "{phase:?}");
        }
        assert_eq!(revert_wait_phase(Decrypting), Decrypting);
        assert_eq!(revert_wait_phase(AwaitingKmsRemoval), AwaitingKmsRemoval);
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
