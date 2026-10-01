use crate::etcd_encryption::{Acknowledgements, EncryptionPhase};

/// Everything the forward state machine needs, gathered by the reconciler.
/// A probe that errored is passed as `false`: failure never advances a phase.
#[derive(Debug, Clone, Copy)]
pub struct PhaseInputs {
    pub current: EncryptionPhase,
    /// The EFFECTIVE acknowledgements (`etcd_encryption::effective_acks`):
    /// an ack counts only if set after its patch was published.
    pub acks: Acknowledgements,
    /// A Ready plugin pod on every control-plane node.
    pub plugin_ready: bool,
    /// The apiserver reports an active KMS provider.
    /// A probe error is passed as `false`: this blocks phase advancement without ambiguity.
    pub kms_active: bool,
    /// A canary Secret written and read back round-trips.
    pub canary_ok: bool,
    /// Every Secret was rewritten with zero failures.
    pub rewrite_complete: bool,
    /// Every Secret can be listed (and therefore decrypted) through the
    /// apiserver right now. A listing error is passed as `false`.
    pub secrets_readable: bool,
}

/// One forward step. Never regresses: a probe going negative after a phase was
/// reached leaves the phase alone (the reconciler reports `Degraded`
/// separately), and an acknowledgement is never reset.
pub fn next_phase(inputs: &PhaseInputs) -> EncryptionPhase {
    use EncryptionPhase::*;
    match inputs.current {
        Pending => InstallingPlugin,
        InstallingPlugin if inputs.plugin_ready => AwaitingKmsConfig,
        AwaitingKmsConfig if inputs.kms_active && inputs.acks.kms_config_applied => Rewriting,
        Rewriting if inputs.rewrite_complete => AwaitingPlaintextRemoval,
        AwaitingPlaintextRemoval
            if inputs.acks.plaintext_removed
                && inputs.kms_active
                && inputs.canary_ok
                && inputs.secrets_readable =>
        {
            Encrypted
        }
        other => other,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CleanupInputs {
    pub phase: EncryptionPhase,
    /// Effective acknowledgements, except `kms_config_applied`, which is the
    /// RAW spec value (see `etcd_encryption_reconciler::cleanup_inputs`).
    pub acks: Acknowledgements,
    /// The apiserver reports an active KMS provider.
    /// `Some(true)` or `Some(false)` = probe result; `None` = probe failed/unknown.
    /// A failed probe MUST be `None` (never true): never remove the plugin while its status is unknown.
    pub kms_active: Option<bool>,
    /// Every Secret can be listed (and therefore decrypted) through the
    /// apiserver right now. A listing error is passed as `false`: it never
    /// permits plugin removal.
    pub secrets_readable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStep {
    /// Nothing depends on the plugin (or it is no longer in use): delete it.
    RemovePlugin,
    /// Publish the revert patch (identity first, kms second); wait for `kmsReverted`.
    AwaitRevertAck,
    /// Rewrite every Secret so none is left that only the plugin can read.
    Decrypt,
    /// Publish the remove-kms patch; wait for `kmsRemoved` and no KMS provider.
    AwaitKmsRemoval,
}

/// Which deletion step to run. The finalizer must keep returning an error for
/// every step except `RemovePlugin`: removing the plugin while the apiserver
/// still depends on it leaves the apiserver unable to read Secrets.
///
/// Engaged-ness (does the apiserver possibly depend on the plugin?) errs on
/// the safe side:
/// - `Pending` / `InstallingPlugin`: engaged unless the apiserver positively
///   reports no KMS provider (`kms_active == Some(false)`). The operator may
///   have applied a patch out of band; an unreadable probe keeps the plugin.
/// - `AwaitingKmsConfig`: engaged if `kmsConfigApplied` is set (the RAW spec
///   value, not the effective one) or the probe does not say "no KMS".
/// - every later phase: engaged.
///
/// `acks` other than `kms_config_applied` must be the effective ones (see
/// `etcd_encryption::effective_acks`). The plugin is removed at the end only
/// when `kmsRemoved` counts, the apiserver reports no KMS provider, and every
/// Secret can be read.
pub fn cleanup_step(inputs: &CleanupInputs) -> CleanupStep {
    use EncryptionPhase::*;
    let engaged = match inputs.phase {
        Pending | InstallingPlugin => inputs.kms_active != Some(false),
        AwaitingKmsConfig => inputs.acks.kms_config_applied || inputs.kms_active != Some(false),
        _ => true,
    };
    if !engaged {
        return CleanupStep::RemovePlugin;
    }
    if !inputs.acks.kms_reverted {
        return CleanupStep::AwaitRevertAck;
    }
    if inputs.phase != AwaitingKmsRemoval {
        return CleanupStep::Decrypt;
    }
    if inputs.acks.kms_removed && inputs.kms_active == Some(false) && inputs.secrets_readable {
        return CleanupStep::RemovePlugin;
    }
    CleanupStep::AwaitKmsRemoval
}

#[cfg(test)]
mod tests {
    use super::*;
    use EncryptionPhase::*;

    fn inputs(current: EncryptionPhase) -> PhaseInputs {
        PhaseInputs {
            current,
            acks: Acknowledgements::default(),
            plugin_ready: false,
            kms_active: false,
            canary_ok: false,
            rewrite_complete: false,
            secrets_readable: false,
        }
    }

    #[test]
    fn pending_always_moves_to_installing_the_plugin() {
        assert_eq!(next_phase(&inputs(Pending)), InstallingPlugin);
    }

    #[test]
    fn installing_waits_for_the_plugin_on_every_control_plane_node() {
        assert_eq!(next_phase(&inputs(InstallingPlugin)), InstallingPlugin);
        let ready = PhaseInputs { plugin_ready: true, ..inputs(InstallingPlugin) };
        assert_eq!(next_phase(&ready), AwaitingKmsConfig);
    }

    #[test]
    fn awaiting_kms_config_needs_both_the_probe_and_the_acknowledgement() {
        let only_probe = PhaseInputs { kms_active: true, ..inputs(AwaitingKmsConfig) };
        assert_eq!(next_phase(&only_probe), AwaitingKmsConfig);

        let only_ack = PhaseInputs {
            acks: Acknowledgements { kms_config_applied: true, ..Default::default() },
            ..inputs(AwaitingKmsConfig)
        };
        assert_eq!(next_phase(&only_ack), AwaitingKmsConfig);

        let both = PhaseInputs { kms_active: true, ..only_ack };
        assert_eq!(next_phase(&both), Rewriting);
    }

    #[test]
    fn rewriting_completes_only_when_every_secret_was_rewritten() {
        assert_eq!(next_phase(&inputs(Rewriting)), Rewriting);
        let done = PhaseInputs { rewrite_complete: true, ..inputs(Rewriting) };
        assert_eq!(next_phase(&done), AwaitingPlaintextRemoval);
    }

    #[test]
    fn plaintext_removal_needs_ack_probe_and_canary() {
        let acks = Acknowledgements { kms_config_applied: true, plaintext_removed: true, ..Default::default() };
        let base = PhaseInputs { acks, ..inputs(AwaitingPlaintextRemoval) };

        assert_eq!(next_phase(&base), AwaitingPlaintextRemoval);
        assert_eq!(next_phase(&PhaseInputs { kms_active: true, ..base }), AwaitingPlaintextRemoval);
        assert_eq!(next_phase(&PhaseInputs { canary_ok: true, ..base }), AwaitingPlaintextRemoval);
        assert_eq!(
            next_phase(&PhaseInputs { kms_active: true, canary_ok: true, secrets_readable: true, ..base }),
            Encrypted
        );
        let no_ack = PhaseInputs {
            kms_active: true,
            canary_ok: true,
            secrets_readable: true,
            acks: Acknowledgements::default(),
            ..base
        };
        assert_eq!(next_phase(&no_ack), AwaitingPlaintextRemoval);
    }

    #[test]
    fn plaintext_removal_is_not_encrypted_while_any_secret_is_unreadable() {
        let acks = Acknowledgements { kms_config_applied: true, plaintext_removed: true, ..Default::default() };
        let unreadable = PhaseInputs {
            acks,
            kms_active: true,
            canary_ok: true,
            secrets_readable: false,
            ..inputs(AwaitingPlaintextRemoval)
        };

        assert_eq!(next_phase(&unreadable), AwaitingPlaintextRemoval);
    }

    #[test]
    fn the_phase_never_regresses_when_probes_go_negative() {
        let degraded = PhaseInputs { plugin_ready: false, kms_active: false, canary_ok: false, ..inputs(Encrypted) };
        assert_eq!(next_phase(&degraded), Encrypted);
        let degraded = PhaseInputs { kms_active: false, ..inputs(Rewriting) };
        assert_eq!(next_phase(&degraded), Rewriting);
    }

    #[test]
    fn deletion_phases_are_not_advanced_by_the_forward_machine() {
        for phase in [RevertingKms, Decrypting, AwaitingKmsRemoval] {
            let all_true = PhaseInputs {
                acks: Acknowledgements { kms_config_applied: true, plaintext_removed: true, kms_reverted: true, kms_removed: true },
                plugin_ready: true,
                kms_active: true,
                canary_ok: true,
                rewrite_complete: true,
                secrets_readable: true,
                ..inputs(phase)
            };
            assert_eq!(next_phase(&all_true), phase);
        }
    }

    /// Secrets readable: the gate under test elsewhere is the protocol itself.
    fn cleanup(phase: EncryptionPhase, acks: Acknowledgements, kms_active: Option<bool>) -> CleanupStep {
        cleanup_step(&CleanupInputs { phase, acks, kms_active, secrets_readable: true })
    }

    #[test]
    fn nothing_depends_on_the_plugin_before_patch_1_is_acknowledged() {
        let none = Acknowledgements::default();
        assert_eq!(cleanup(Pending, none, Some(false)), CleanupStep::RemovePlugin);
        assert_eq!(cleanup(InstallingPlugin, none, Some(false)), CleanupStep::RemovePlugin);
        assert_eq!(cleanup(AwaitingKmsConfig, none, Some(false)), CleanupStep::RemovePlugin);
    }

    #[test]
    fn after_patch_1_is_acknowledged_the_plugin_must_stay_until_the_revert_finishes() {
        // Review Focus 5.
        let applied = Acknowledgements { kms_config_applied: true, ..Default::default() };
        for phase in [AwaitingKmsConfig, Rewriting, AwaitingPlaintextRemoval, Encrypted, RevertingKms] {
            assert_eq!(cleanup(phase, applied, Some(true)), CleanupStep::AwaitRevertAck, "{phase:?}");
        }
    }

    #[test]
    fn once_reverted_the_secrets_are_rewritten_then_kms_removal_is_awaited() {
        let reverted = Acknowledgements { kms_config_applied: true, kms_reverted: true, ..Default::default() };

        assert_eq!(cleanup(RevertingKms, reverted, Some(true)), CleanupStep::Decrypt);
        assert_eq!(cleanup(Decrypting, reverted, Some(true)), CleanupStep::Decrypt);
        assert_eq!(cleanup(AwaitingKmsRemoval, reverted, Some(true)), CleanupStep::AwaitKmsRemoval);
    }

    #[test]
    fn the_plugin_is_removed_only_after_kmsremoved_and_no_kms_provider_is_active() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };

        assert_eq!(cleanup(AwaitingKmsRemoval, removed, Some(true)), CleanupStep::AwaitKmsRemoval);
        assert_eq!(cleanup(AwaitingKmsRemoval, removed, Some(false)), CleanupStep::RemovePlugin);
    }

    #[test]
    fn kmsremoved_does_not_skip_the_decrypt_step() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };

        assert_eq!(cleanup(Decrypting, removed, Some(false)), CleanupStep::Decrypt);
    }

    #[test]
    fn awaiting_kms_config_without_ack_depends_on_kms_probe_state() {
        let none = Acknowledgements::default();
        // (a) With kms_active Some(true) and no ack: plugin is engaged, waiting for revert
        assert_eq!(cleanup(AwaitingKmsConfig, none, Some(true)), CleanupStep::AwaitRevertAck);
        // (b) With kms_active None (failed/unknown) and no ack: not disengaged, still waiting
        assert_eq!(cleanup(AwaitingKmsConfig, none, None), CleanupStep::AwaitRevertAck);
        // (c) With kms_active Some(false) and no ack: disengaged, can remove
        assert_eq!(cleanup(AwaitingKmsConfig, none, Some(false)), CleanupStep::RemovePlugin);
    }

    #[test]
    fn awaiting_kms_removal_fails_safe_on_unknown_kms_probe() {
        // (d) Even with all acks, if kms_active is None (probe failed), stay in AwaitKmsRemoval
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };
        assert_eq!(cleanup(AwaitingKmsRemoval, removed, None), CleanupStep::AwaitKmsRemoval);
    }

    #[test]
    fn rewriting_and_encrypted_with_no_acks_wait_for_revert() {
        // (e) Even when no acks are set, engaged phases wait for kms_reverted
        let none = Acknowledgements::default();
        assert_eq!(cleanup(Rewriting, none, Some(true)), CleanupStep::AwaitRevertAck);
        assert_eq!(cleanup(Encrypted, none, Some(true)), CleanupStep::AwaitRevertAck);
    }

    #[test]
    fn awaiting_kms_removal_without_revert_ack_blocks_removal() {
        // (f) Even if kms_removed is true, without kms_reverted, stay at AwaitRevertAck
        let no_revert = Acknowledgements { kms_config_applied: true, kms_removed: true, ..Default::default() };
        assert_eq!(cleanup(AwaitingKmsRemoval, no_revert, Some(false)), CleanupStep::AwaitRevertAck);
    }

    #[test]
    fn pending_and_installing_are_engaged_unless_the_apiserver_reports_no_kms() {
        // (g) The operator may have applied a patch out of band before the
        // phase caught up: only a positive "no KMS provider" frees the plugin.
        let none = Acknowledgements::default();
        for phase in [Pending, InstallingPlugin] {
            assert_eq!(cleanup(phase, none, None), CleanupStep::AwaitRevertAck, "{phase:?} unknown probe");
            assert_eq!(cleanup(phase, none, Some(true)), CleanupStep::AwaitRevertAck, "{phase:?} KMS active");
            assert_eq!(cleanup(phase, none, Some(false)), CleanupStep::RemovePlugin, "{phase:?} KMS off");
        }
    }

    #[test]
    fn the_plugin_is_not_removed_while_any_secret_is_unreadable() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };
        let step = cleanup_step(&CleanupInputs {
            phase: AwaitingKmsRemoval,
            acks: removed,
            kms_active: Some(false),
            secrets_readable: false,
        });

        assert_eq!(step, CleanupStep::AwaitKmsRemoval);
    }
}
