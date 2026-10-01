use crate::etcd_encryption::{Acknowledgements, EncryptionPhase};

/// Everything the forward state machine needs, gathered by the reconciler.
/// A probe that errored is passed as `false`: failure never advances a phase.
#[derive(Debug, Clone, Copy)]
pub struct PhaseInputs {
    pub current: EncryptionPhase,
    pub acks: Acknowledgements,
    /// A Ready plugin pod on every control-plane node.
    pub plugin_ready: bool,
    /// The apiserver reports an active KMS provider.
    pub kms_active: bool,
    /// A canary Secret written and read back round-trips.
    pub canary_ok: bool,
    /// Every Secret was rewritten with zero failures.
    pub rewrite_complete: bool,
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
            if inputs.acks.plaintext_removed && inputs.kms_active && inputs.canary_ok =>
        {
            Encrypted
        }
        other => other,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CleanupInputs {
    pub phase: EncryptionPhase,
    pub acks: Acknowledgements,
    pub kms_active: bool,
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
pub fn cleanup_step(inputs: &CleanupInputs) -> CleanupStep {
    use EncryptionPhase::*;
    let engaged = !matches!(inputs.phase, Pending | InstallingPlugin)
        && !(inputs.phase == AwaitingKmsConfig && !inputs.acks.kms_config_applied);
    if !engaged {
        return CleanupStep::RemovePlugin;
    }
    if !inputs.acks.kms_reverted {
        return CleanupStep::AwaitRevertAck;
    }
    if inputs.phase != AwaitingKmsRemoval {
        return CleanupStep::Decrypt;
    }
    if inputs.acks.kms_removed && !inputs.kms_active {
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
        assert_eq!(next_phase(&PhaseInputs { kms_active: true, canary_ok: true, ..base }), Encrypted);
        let no_ack = PhaseInputs { kms_active: true, canary_ok: true, acks: Acknowledgements::default(), ..base };
        assert_eq!(next_phase(&no_ack), AwaitingPlaintextRemoval);
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
                ..inputs(phase)
            };
            assert_eq!(next_phase(&all_true), phase);
        }
    }

    fn cleanup(phase: EncryptionPhase, acks: Acknowledgements, kms_active: bool) -> CleanupStep {
        cleanup_step(&CleanupInputs { phase, acks, kms_active })
    }

    #[test]
    fn nothing_depends_on_the_plugin_before_patch_1_is_acknowledged() {
        let none = Acknowledgements::default();
        assert_eq!(cleanup(Pending, none, false), CleanupStep::RemovePlugin);
        assert_eq!(cleanup(InstallingPlugin, none, false), CleanupStep::RemovePlugin);
        assert_eq!(cleanup(AwaitingKmsConfig, none, false), CleanupStep::RemovePlugin);
    }

    #[test]
    fn after_patch_1_is_acknowledged_the_plugin_must_stay_until_the_revert_finishes() {
        // Review Focus 5.
        let applied = Acknowledgements { kms_config_applied: true, ..Default::default() };
        for phase in [AwaitingKmsConfig, Rewriting, AwaitingPlaintextRemoval, Encrypted, RevertingKms] {
            assert_eq!(cleanup(phase, applied, true), CleanupStep::AwaitRevertAck, "{phase:?}");
        }
    }

    #[test]
    fn once_reverted_the_secrets_are_rewritten_then_kms_removal_is_awaited() {
        let reverted = Acknowledgements { kms_config_applied: true, kms_reverted: true, ..Default::default() };

        assert_eq!(cleanup(RevertingKms, reverted, true), CleanupStep::Decrypt);
        assert_eq!(cleanup(Decrypting, reverted, true), CleanupStep::Decrypt);
        assert_eq!(cleanup(AwaitingKmsRemoval, reverted, true), CleanupStep::AwaitKmsRemoval);
    }

    #[test]
    fn the_plugin_is_removed_only_after_kmsremoved_and_no_kms_provider_is_active() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };

        assert_eq!(cleanup(AwaitingKmsRemoval, removed, true), CleanupStep::AwaitKmsRemoval);
        assert_eq!(cleanup(AwaitingKmsRemoval, removed, false), CleanupStep::RemovePlugin);
    }

    #[test]
    fn kmsremoved_does_not_skip_the_decrypt_step() {
        let removed = Acknowledgements { kms_config_applied: true, kms_reverted: true, kms_removed: true, ..Default::default() };

        assert_eq!(cleanup(Decrypting, removed, false), CleanupStep::Decrypt);
    }
}
