---
name: etcd-encryption-2026-09
description: EtcdEncryption slice, 2026-09-30 - seventh CRD; operator-acknowledged Talos-patch protocol because the controller never holds a Talos credential; Barbican first, other providers designed for; NOT live-verified yet
metadata:
  type: project
---

`EtcdEncryption` (cluster-scoped singleton `default`, shortname `etcdenc`) installs the Barbican KMS plugin, re-encrypts every Secret through KMS and walks the cluster to no plaintext Secrets in etcd. Spec: `docs/superpowers/specs/2026-09-30-etcd-encryption-design.md`; plan: `docs/superpowers/plans/2026-09-30-etcd-encryption.md`; acceptance: `docs/runbooks/etcd-encryption-verification.md`.

**Why a protocol, not just a reconcile:** on Talos the apiserver config lives in the machine config, applied through the Talos API, which this controller never touches. The controller publishes the exact patches in `status.talosPatches`, the operator applies them, and `spec.acknowledgements.*` are the gates. The controller also probes (apiserver `/metrics` + a canary Secret) and advances only when probe and acknowledgement agree.

**Non-obvious facts:**
- No `keyId` field: the Barbican plugin reads the key only from `cloud.conf`'s `[KeyManager] key-id`.
- Deleting is three operator steps, not one: revert patch (identity FIRST, kms second) -> rewrite all Secrets -> remove-kms patch -> only then delete the plugin. Identity-only straight away would make every KMS-encrypted Secret unreadable.
- No `Failed` phase: position in the protocol must survive failures, so failures are a `Ready=False` condition.
- The finalizer's Cleanup arm must never return `Ok` while the plugin is still needed (kube's finalizer strips the finalizer on any `Ok`).
- Probes are fail-safe in opposite directions: on the forward path an unreadable probe means "not active" (blocks advancing); during deletion it means "unknown" (`Option<bool>` None), which keeps the CR Terminating and never removes the plugin.
- The forward rewrite runs only while the apiserver reports an active KMS provider (rewriting under identity would strand Secrets once identity is removed). Plugin readiness needs a current DaemonSet rollout and a Ready pod on every control-plane node.
- Deleting in `AwaitingKmsConfig` with patch 1 applied but not acknowledged still walks the revert protocol if the apiserver already reports KMS active.
- The plugin DaemonSet is built in Rust, not rendered from a chart; upstream ships only a raw `ds.yaml`.
- Unverified until the runbook is run: the `apiserver_envelope_encryption_*` metric names, the Talos `KubeEtcdEncryptionConfig` KMS block and socket-mount shape (and that `talosctl patch machineconfig` accepts the two-document patch), the image tag.
- Not built: Azure, AWS, GCP, OCI providers (one builder + one enum variant each), a Talos-API opt-in, key management, encrypting non-Secret resources.

Related: [[rbac-cluster-admin-tradeoff]], [[wait-for-crd-established]], [[cloud-controller-manager-2026-09]].
