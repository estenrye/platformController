# Clean Up Applied Resources on CniInstallation Delete

Status: Approved for planning
Date: 2026-09-19

## Purpose

A real install on a live Talos cluster confirmed the third of three issues found during Phase 5 testing: deleting a `CniInstallation` does not clean up anything the controller applied. There is no Kubernetes finalizer on the `CniInstallation` CRD, so `kubectl delete cniinstallation default` simply removes the custom resource itself — the `tigera-operator` namespace, its CRDs, its RBAC, its Deployment, and the `Installation`/`APIServer` custom resources it manages are all left behind, orphaned with no controller left tracking them.

This is the last of the three issues from that bug report to be addressed (Issue 1, FlexVolume, and Issue 2, the CRD-established race, are both already merged). Issue 4 (IPv6-only nodes and unconditional IPv4 autodetection) remains explicitly out of scope, deferred to its own future brainstorming session.

## Non-goals

- Issue 4 (IPv4 autodetection on IPv6-only nodes) — unrelated, tracked separately.
- Any change to the `reconcile`/apply path beyond what's needed to add the finalizer wrapper. `reconcile`'s own logic (render, apply, prune, status) is untouched.
- Guaranteeing zero orphaned resources under every possible failure mode (e.g. the controller crash-looping indefinitely, or a cluster-admin manually removing the finalizer). The finalizer plus a bounded, retried wait gets this to the same "self-heals via retry" standard the rest of this controller already holds itself to — not an unconditional guarantee.

## Design

### 1. Wrap `reconcile` with `kube::runtime::finalizer`

`main.rs` currently calls `reconcile` directly: `Controller::for_stream(installations, reader).run(reconcile, error_policy, context)`. This changes to a new `reconciler::reconcile_with_finalizer`, which builds `Api<CniInstallation>` and calls `kube::runtime::finalizer(&api, FINALIZER_NAME, obj, |event| async { ... })`, dispatching:

- `Event::Apply(obj)` → the existing `reconcile(obj, ctx)`, unchanged.
- `Event::Cleanup(obj)` → a new `cleanup(obj, ctx)`.

`FINALIZER_NAME = "platform.rye.ninja/cleanup"`, matching the CRD's own API group.

`error_policy`'s signature changes from `fn error_policy(_: Arc<CniInstallation>, _: &ReconcileError, _: Arc<Context>) -> Action` to accept `&kube::runtime::finalizer::Error<ReconcileError>` instead. All of that enum's variants (`ApplyFailed`, `CleanupFailed`, `AddFinalizer`, `RemoveFinalizer`, `UnnamedObject`, `InvalidFinalizer`) requeue at the existing 30s — no differentiated handling, matching today's uniform behavior.

### 2. Fix a watch-pipeline gap this surfaces

`main.rs`'s watch stream filters on `predicates::generation` only (added to stop `update_status`'s own patches from re-triggering reconcile, since status-subresource writes don't bump `metadata.generation`). Verified directly against `kube-runtime` 4.2.0's source: `predicates::generation` hashes only `obj.meta().generation`, and a plain `kubectl delete` on an object with a finalizer only sets `metadata.deletionTimestamp` — also not a `spec` write, so `generation` doesn't change either. (`kube-runtime`'s own `finalizer.rs` checks deletion via the completely separate `obj.meta().deletion_timestamp.is_some()`.) Left as-is, the current pipeline would silently drop the watch event signalling "this object was just deleted," and `cleanup` wouldn't run until the next periodic resync — up to 5 minutes later, not on `kubectl delete`.

Fix: add `fn deletion_requested(obj: &CniInstallation) -> Option<u64>`, returning a hash only when `deletion_timestamp` is set, and combine it with the existing predicate using `kube-runtime`'s own composition method: `predicates::generation.combine(deletion_requested)`.

### 3. CRD field: `spec.cleanupTimeoutSeconds`

Waiting for `tigera-operator` to actually finish tearing down Calico's own resources (the `calico-node` DaemonSet, etc. — none of which are in `status.appliedResources`, since this controller didn't apply them directly) before removing the operator itself needs a bounded timeout. This is generalized as a top-level, provider-agnostic field on `CniInstallationSpec` — not nested under `CalicoSpec` — since "wait for the provider's own managed resources to actually disappear during cleanup" isn't a Calico-specific concept; any future CNI provider's `Cleanup` would need something equivalent, per `goals.md`'s multi-provider vision.

```rust
pub struct CniInstallationSpec {
    pub platform_kind: PlatformKind,
    pub provider: CniProvider,
    pub calico: CalicoSpec,
    #[serde(default = "default_cleanup_timeout_seconds")]
    pub cleanup_timeout_seconds: u32,
}

fn default_cleanup_timeout_seconds() -> u32 {
    60
}
```

Defaults to 60 seconds when omitted, so existing manifests (including `examples/cni-installation.yaml`) need no changes. A user who wants the simpler "just delete, don't wait for the operator" behavior can set this near 0 without needing a separate code path — the wait loop degenerates to "one poll, then proceed" rather than being a distinct mode.

### 4. Cleanup sequencing

Reuse the same rank classification `manifests::apply_rank` already uses for ordering applies, refactored into a shared `rank_for_kind(kind: &str) -> u8` (since `cleanup` only has `AppliedResourceRef` values from `status.appliedResources`, not full `DynamicObject`s, to classify). Objects at the highest rank (5 — the catch-all bucket, which is exactly where `Installation`/`APIServer` custom resources land, since they're not one of the enumerated infra kinds) are the provider's own managed inputs; everything else (Namespace, CRDs, RBAC, the operator's Deployment) is infrastructure this controller applied directly.

`cleanup(obj, ctx)`:

1. Same `leader_gate` check `reconcile` already uses — a non-leader replica requeues without touching anything.
2. Read `obj.status.appliedResources` (empty if `status` is `None` — `kube-runtime`'s own finalizer docs explicitly require `Cleanup` to tolerate `Apply` never having run, e.g. a `CniInstallation` created and deleted before its first successful reconcile). If empty, return immediately so the finalizer comes off right away.
3. Delete the rank-5 objects first, **waiting for each to actually disappear** (poll until absent or `spec.cleanupTimeoutSeconds` elapses) before proceeding — this is what gives the provider's operator room to react to its custom resource being deleted and tear down its own managed resources.
4. Delete everything else (ranks 0-4) in reverse apply order — fire-and-forget, matching the existing prune-on-reconcile `delete_object` semantics already used elsewhere in this file; nothing downstream depends on these completing before the next one.
5. Return `Action::await_change()`; `kube-runtime` removes the finalizer once this returns `Ok`.

**New code in `src/apply.rs`:**

- `delete_and_wait_for_removal(client, reference, timeout) -> Result<(), ApplyError>` — issues the delete via the existing `delete_object`, then polls `get_opt` bounded by `tokio::time::timeout_at` (applying the lesson from Issue 2's final review — a networked poll's underlying API call must itself be wall-clock-bounded, not just the surrounding loop — from the start this time). New `ApplyError::NotDeleted { kind: String, name: String, timeout: Duration }` variant.
- A small refactor: `delete_object`'s existing discovery + `Api<DynamicObject>` construction is factored into a shared private helper so `delete_and_wait_for_removal` doesn't duplicate it.

**Retries are naturally idempotent:** if `cleanup` fails partway (a transient API error deleting one object), the finalizer stays, `error_policy` requeues at 30s, and the next attempt re-reads `status.appliedResources` (untouched by `cleanup`, which never writes status) and redoes the whole sequence — already-deleted objects are tolerated everywhere (`delete_object` already treats 404 as success), so this just repeats work harmlessly rather than needing separate resume-state tracking.

## Testing

- **Unit**: the rank-based partitioning logic (which resources wait vs. don't), as a small pure function tested with hand-built `AppliedResourceRef` values across ranks; the new `deletion_requested` predicate with hand-built `CniInstallation` values (no `deletionTimestamp` → filtered, with one → passes), in the same style `kube-runtime`'s own predicate tests use; `cleanup`'s empty-status and leader-gate short-circuit paths (pure enough to test without a cluster, mirroring `reconcile`'s existing `leader_gate_returns_*` tests); the new `cleanupTimeoutSeconds` field's default-when-omitted behavior, alongside the existing `CniInstallationSpec` round-trip test. `delete_and_wait_for_removal`'s actual polling loop is networked and not unit-tested, matching the established pattern for `wait_for_crd_established`.
- **Live verification** (the step that actually proves this works, same discipline as Issues 1 and 2): on the real Talos cluster, get a `CniInstallation` to `Ready`, then `kubectl delete cniinstallation default` and confirm — `deletionTimestamp` appears immediately (not after a multi-minute delay, proving the watch-pipeline fix), `calico-node` pods actually terminate before the `tigera-operator` Deployment disappears (proving the operator got to react), and the object itself along with every resource this controller applied (namespace, CRDs, RBAC, Deployment) is fully gone afterward. Then re-apply the example `CniInstallation` and confirm a fresh install still reaches `Ready` cleanly — a round-trip check that deletion didn't leave stuck state behind.
