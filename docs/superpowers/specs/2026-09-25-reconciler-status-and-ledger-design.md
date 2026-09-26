# Durable Ledger and Failure Status for the Reconcilers

Status: Draft, awaiting review
Date: 2026-09-25

## Purpose

Two gaps exist in both reconcilers (`CniInstallation` in `src/reconciler.rs`, `PullThroughCache` in `src/cache_reconciler.rs`). The second was known for the CNI reconciler before `PullThroughCache` was written (see `docs/memory/ipv6-only-calico-2026-09.md`); the new reconciler copied both.

**Gap 1: failures after validation are invisible.** `reconcile` writes `status` only on a validation failure (`Failed` plus a reason) or after a fully successful run (`Ready`). A Helm render, manifest parse, apply or prune error returns through `?` and writes nothing. `error_policy` requeues after 30 seconds. The resource's `.status` stays empty or stale, and the only evidence is the controller's log. The most likely first-user error is `chartVersion: v0.7.4` (the OCI chart tag has no `v`): it passes validation, `helm` fails with "not found", and `kubectl get ptc` shows nothing.

**Gap 2: the ledger is saved only at the end.** `status.appliedResources` is what pruning and cleanup-on-delete act on. `reconcile` builds it in memory while applying and saves it once, in the final `Ready` write. A run that fails partway loses it:

- A first install that fails partway and is then deleted has an empty ledger. `cleanup` sees nothing, returns `Ok`, and the finalizer is stripped. The applied objects, including the privileged `spegel` (or `tigera-operator`) namespace, stay in the cluster.
- An update that fails partway leaves objects applied in that run out of the saved ledger, so a later prune or delete does not see them.

Both are silent: the operator learns of them only from a leftover namespace or a missing status.

## Non-goals

- Restructuring the two reconcilers into a shared framework. Only the pure ledger arithmetic is shared (below).
- Health-derived readiness (`Ready` still means "manifests applied").
- New CRD fields or schema changes. `Phase::Installing` already exists in `src/crd.rs` and is currently unused; the condition and status shapes are unchanged, so **no CRD regeneration and no upgrade-ordering constraint**.
- Changing cleanup ordering, cleanup waits or the Calico-specific sweep logic.

## Design

### Invariant

**No object is applied unless it is already in the durable ledger.** Every apply, prune and cleanup decision then reads a ledger that is at least as large as what may exist in the cluster.

An over-inclusive ledger is safe: `apply::delete_object` treats a missing kind or a 404 as success, and `delete_and_wait_for_removal` returns immediately for an object that was never created (both verified in `src/apply.rs`).

### Ledger checkpoint before applying

Everything a reconcile will apply is known before the first apply, because it is a pure function of the spec and the rendered chart:

- `PullThroughCache`: the synthesized namespace plus the parsed, sorted chart objects.
- `CniInstallation`: the synthesized namespace, the parsed, sorted chart objects, then the two Calico phases (`pod_pool_objects`, `routing_and_lb_objects`), which depend only on the spec.

After render and parse, each reconciler computes `desired: Vec<AppliedResourceRef>` in apply order (via `apply::resource_ref`) and, **only if `desired` contains an entry not in the current `status.appliedResources`**, writes a checkpoint before applying anything:

- `appliedResources = merge_ledger(previous, desired)`
- `phase = Installing`, condition `Applied=False`, reason `Applying`

In steady state (every 300 seconds resync, no spec change) `desired` is already covered by the ledger, so nothing extra is written and `status` does not churn. The checkpoint fires on first install and whenever a spec or chart-version change adds objects. If the checkpoint write fails, the reconcile returns the error **before applying anything**, which keeps the invariant.

The final write after a successful apply and prune sets `appliedResources = desired` (dropping pruned entries), as today.

A leader change or a crash at any point is safe: the successor reads a ledger that already covers whatever was applied.

### `merge_ledger`

A pure function in a new module `src/ledger.rs`, shared by both reconcilers:

```rust
/// `desired` in apply order, followed by any `previous` entries not in `desired`.
pub fn merge_ledger(previous: &[AppliedResourceRef], desired: &[AppliedResourceRef]) -> Vec<AppliedResourceRef>;

/// True when `desired` holds an entry missing from `previous`.
pub fn ledger_needs_checkpoint(previous: &[AppliedResourceRef], desired: &[AppliedResourceRef]) -> bool;
```

Order matters: CNI cleanup deletes in reverse ledger order and moves the `Installation` last, so `desired` keeps apply order. Stale entries follow it (so cleanup, which reverses, removes them first; they are being pruned anyway). Duplicates are removed, first occurrence wins. This module is the only code shared between the reconcilers.

### Failure status

Each reconciler's `reconcile` is split into a wrapper and `reconcile_inner`. `reconcile_inner` records `desired` in a small `Progress` value as soon as it is known. The wrapper calls it and, on any error other than `NotLeader` and `Status` (a failed status write cannot be reported through status) and other than the validation error that already writes its own status:

- writes `phase = Failed`, condition `Applied=False`, a reason from the table below, the error text as the message, and `appliedResources = merge_ledger(previous, progress.desired)` (or `previous` if the failure came before `desired` was known);
- then returns the original error so `error_policy` and the log are unchanged.

| Error variant | `reason` |
|---|---|
| `Helm` | `RenderFailed` |
| `Manifest` | `InvalidManifest` |
| `Apply` (incl. kind-availability and CRD-established timeouts, delete failures during prune) | `ApplyFailed` |

Validation reasons are unchanged. `reason()` is added to each reconciler's error enum next to the existing `ValidationError::reason`.

A resource that was `Ready` and then hits a transient failure flips to `Failed` until the next successful reconcile. That is accurate (`Applied=False`), and the ledger it carries is complete, so nothing is lost by the flip. Retries every 30 seconds rewrite the same status; the predicate filter in `main.rs` (generation-based) already ignores status-only writes, so they do not re-trigger reconciles.

### Cleanup

Unchanged. It already deletes everything in the ledger in reverse order; the ledger is now durable, so it acts on what was really applied.

### Documentation

- `docs/runbooks/pull-through-cache-verification.md`: remove the "When something goes wrong" gaps and the `kubectl delete ns spegel` recovery note; say failures now appear in `status`.
- `docs/superpowers/specs/2026-09-25-pull-through-cache-design.md`: replace the two documented gaps with a pointer to this spec.
- `docs/memory/ipv6-only-calico-2026-09.md` (the "if a first install fails partway ... remove them by hand" sentence) and `docs/memory/pull-through-cache-2026-09.md`: mark the gaps fixed and link this spec.
- The example manifest comment about checking controller logs for a wrong `chartVersion` is reworded to point at `status`.

## Testing

- **Unit (`src/ledger.rs`):** `merge_ledger` ordering, de-duplication, stale-after-desired; `ledger_needs_checkpoint` true/false including equal sets and a reordered set.
- **Unit (each reconciler):** the error-to-reason mapping for every variant.
- **Existing tests:** every CNI reconciler test must pass unchanged (cleanup ordering, sweep, partition).
- **Live acceptance on a real cluster (both must pass):**
  1. *Render failure:* apply a `PullThroughCache` with `chartVersion: v0.7.4`. Expect `phase: Failed`, reason `RenderFailed`, message naming the helm error, within one reconcile; correcting it to `0.7.4` reaches `Ready`.
  2. *Partial apply, then delete:* apply a `PullThroughCache` with `helmValues: {resources: {limits: {memory: "bogus"}}}` so the API server rejects the DaemonSet after the namespace, ServiceAccount and Services were applied. Expect `Failed`/`ApplyFailed` with `appliedResources` listing the objects planned; then delete the resource and expect the `spegel` namespace to be gone (today it is left behind).
  3. *CNI regression:* the existing cleanup steps in `docs/runbooks/ipv6-only-kvm-verification.md` still pass, and a fresh `CniInstallation` still ends with exactly the declared pools.

## Rollout risk

This edits the CNI reconcile path that was live-verified after several cleanup fixes. Mitigations: the change is additive around the existing apply sequence (a checkpoint before it, a wrapper after it); cleanup and prune code are untouched; every existing test is a regression gate; the CNI live check is part of acceptance. If the CNI change proves contentious, the `PullThroughCache` half can ship alone and the two reconcilers will differ only in this behaviour until the CNI half lands.

## Open questions for the reviewer

1. **Ship both at once, or `PullThroughCache` first?** Recommended: both, to keep the two reconcilers aligned.
2. **Should a transient failure flip a `Ready` resource to `Failed`?** Recommended: yes (accurate and self-healing). The alternative keeps `phase: Ready` and only sets `Applied=False`, which reads as contradictory.
3. **Reason vocabulary** (`RenderFailed`, `InvalidManifest`, `ApplyFailed`, `Applying`): acceptable, or should it follow an existing convention elsewhere in your platform?
