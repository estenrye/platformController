---
name: etcd-encryption-2026-09
description: EtcdEncryption slice, reworked 2026-10-01 from a fresh-install protocol to adopt mode - observe each control-plane apiserver, optionally rewrite Secrets, prove no legacy provider remains; stateless derived phase, no finalizer; EXPERIMENTAL, NOT live-verified
metadata:
  type: project
---

`EtcdEncryption` (cluster-scoped singleton `default`, shortname `etcdenc`) now **adopts** a cluster whose apiserver already uses a KMS provider: it observes each control-plane apiserver, optionally rewrites every Secret, and proves per apiserver that nothing is stored under a legacy provider before the operator removes it. Spec: `docs/superpowers/specs/2026-10-01-etcd-encryption-adopt-design.md` (supersedes `2026-09-30-etcd-encryption-design.md`, the v0.1.11 fresh-install flow); acceptance: `docs/runbooks/etcd-encryption-verification.md`. EXPERIMENTAL, not live-verified.

**Live findings of 2026-10-01 (why v0.1.11's premise was wrong):**
- The Barbican KMS plugin already ran as Talos **static pods** (`barbican-kms-plugin-<node>`) sharing `/var/lib/kms/kms.sock`.
- A DaemonSet plugin has a circular dependency: its pods mount a credentials Secret that is itself KMS-encrypted, so after a control-plane cold start nothing can decrypt it and the plugin can never start. Static pods read config from node disk.
- Readiness shows `[+]kms-providers ok` in `/readyz?verbose`.
- Talos's default secretbox was kept as a **read fallback**; the v0.1.11 "enable KMS" patch would have dropped it and locked 86 Secrets out.
- Apiserver metrics (`apiserver_storage_transformation_operations_total{resource="secrets",status="OK"}`): `to_storage` `k8s:enc:kms:v2:barbican:` = 1; `from_storage` `k8s:enc:kms:v2:barbican:` = 284, `k8s:enc:secretbox:v1:` = 86, `key2:` = 86.
- `key2:` is a per-key sub-prefix of the same secretbox operation; the controller counts only **top-level** `k8s:enc:` prefixes so reads are not double-counted.
- The `apiserver_envelope_encryption_*` family has a bare gauge whenever a provider is *configured*, so it cannot show that KMS is in use (the v0.1.11 probe's mistake).

**The redesign:**
- Phase (`Observing | NotConfigured | Migrating | ReadyToRemoveLegacy | Verified`) is **derived each reconcile** from current evidence plus the spec (`encryption_verdict::derive`): stateless, no persisted protocol position, no acknowledgement-after-publication, no ledger, no `patchGenerations`, no `Failed` phase (validation failure is `Ready=False`/`InvalidSpec`).
- **Per-apiserver checks** (dials each control-plane Node's `InternalIP:6443` with the SA token, TLS server name `kubernetes.default.svc`): writer check (canary write, `to_storage` delta says which provider writes) and reader check (limit-paged list of every Secret, `from_storage` delta by prefix; complete only if deltas sum to at least the number listed). Concurrent readers can make it look worse, not cleaner; the spec's "Residual risk" says it is evidence, not proof, and the runbook demands a quiet-cluster check and an etcd snapshot before removing a provider.
- Tightening: an incomplete/errored reader check never triggers a rewrite (it would repeat full rewrites every 30 s unconfirmably); it is `Observing`.
- `Ready.lastTransitionTime` is refreshed every reconcile, so an old one means the controller is not verifying and the phase is stale.
- **No finalizer, no cleanup, no ledger.** v0.1.11 CRs carry `platform.rye.ninja/cleanup`; the reconciler strips only that finalizer once (and stops if already deleting). Delete the old CR before applying the new schema.
- A Secret that permanently fails to rewrite (e.g. an admission webhook) keeps `status.rewrite.failed > 0`, phase `Migrating`, and repeats the cluster-wide rewrite every 30 s; the log names namespace/name only.
- `Verified` means probes agree + canary round-trips + operator set `legacyProvidersRemoved`; metrics cannot prove the config no longer lists the provider.
- Requeue: `Observing`/`Migrating` 30 s; `NotConfigured`/`ReadyToRemoveLegacy`/`Verified` 600 s. All status counters are `i64` (`u64` emits a CRD format warning).

**What was deleted and why:** `kms_provider.rs`, `kms_barbican.rs`, `talos_patches.rs`, the plugin DaemonSet, the Talos-patch publication and `status.talosPatches`, the acknowledgement protocol (`kmsConfigApplied`, `plaintextRemoved`, `kmsReverted`, `kmsRemoved`) and `patchGenerations`, the finalizer/cleanup/delete protocol, the `keyId`-style provider config, and the `provider`/`barbican` spec fields. Why: the premise (fresh install, controller-managed plugin) was wrong for real clusters, the DaemonSet could never cold-start, the patches would have dropped secretbox, and the v0.1.11 final review's open Critical, minor bugs and wording problems all lived in that machinery. Kept: `secret_rewrite.rs`, the canary Secret, leader gating.

**Open verification items (runbook section 7):** (a) TLS to `https://<node-ip>:6443` with server name `kubernetes.default.svc` (certificate SANs unverified); (b) the exact `kms-providers` readiness name; (c) that a limit-paged list reads from etcd rather than the watch cache; (d) what `transformer_prefix` an identity/plaintext read reports (the design treats empty as legacy); (e) whether a zero-value counter series is absent (parser treats absent as 0). The etcd-snapshot inspection command in the runbook is also unverified.

Not built: installing the plugin, Talos patches, key management, a configurable apiserver port (fixed 6443), non-Secret resources, other providers' specifics (it is provider-agnostic: `kmsProviderName` is a string).

Related: [[rbac-cluster-admin-tradeoff]], [[wait-for-crd-established]], [[cloud-controller-manager-2026-09]].
