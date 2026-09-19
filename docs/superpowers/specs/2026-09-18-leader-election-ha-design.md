# Leader-Election / Multi-Replica HA for platform-controller

Status: Approved for planning
Date: 2026-09-18

## Purpose

The `platform-controller` currently runs as a single replica (`deploy/bootstrap.yaml`), with no coordination mechanism if scaled. This is a real gap against [docs/goals.md](../../goals.md)'s requirement that the operator "be highly available, ensuring that it continues to operate correctly even if some of its instances fail or become unreachable."

This spec adds leader election so the controller can run multiple replicas safely: exactly one replica actively reconciles at a time (the leader); the rest are hot standbys that take over quickly if the leader's pod or node fails.

## Non-goals

- **Active-active work-sharding across replicas.** The controller currently reconciles a single singleton `CniInstallation`. Even once more CRD instances exist (per goals.md's `CloudUnderlay` vision), a single leader process already reconciles distinct objects concurrently — `kube-rs`'s `Controller` runs one async task per distinct object within one process. True multi-replica active-active partitioning of work is a separate, much larger problem with no concrete requirements yet (object counts, throughput targets, whether single-process concurrency actually falls short) and is explicitly deferred.
- Dynamic rebalancing on replica scale-up/down beyond what the Lease protocol naturally provides (a new replica simply competes for the Lease like any other).
- Cross-namespace or cross-cluster leader election.
- Fine-grained mid-reconcile cancellation or drain logic. The design in this spec deliberately achieves "let in-flight work finish" through a much simpler mechanism (Section 2) rather than explicit cancellation/drain machinery.

## 1. Leader-Election Protocol

Uses the built-in `coordination.k8s.io/v1 Lease` resource — no new CRD.

- **Lease identity:** name `platform-controller-leader`, namespace `platform-system`.
- **Holder identity:** the pod's own name, injected via a `POD_NAME` environment variable (downward API, `fieldRef: fieldPath: metadata.name`).
- **Timings** (matching the conventional values used by Kubernetes' own leader-elected components — `kube-scheduler`, `kube-controller-manager` — so no new numbers need to be invented or tuned):
  - `leaseDurationSeconds: 15`
  - `renewDeadlineSeconds: 10`
  - `retryPeriodSeconds: 2`
- **Protocol**, run as a background task in every replica:
  1. `get` the Lease. If it doesn't exist, attempt to `create` it with self as holder and `renewTime = now`.
  2. If it exists and is expired (`now > renewTime + leaseDurationSeconds`), attempt an optimistic-concurrency `update` (using the fetched `resourceVersion`) claiming holder = self. A `409 Conflict` means another replica won the race first — back off (`retryPeriodSeconds`) and re-check.
  3. If it exists, is not expired, and is held by someone else: sleep `retryPeriodSeconds` and re-check.
  4. While holding the Lease: on a timer (roughly every `retryPeriodSeconds` while inside `renewDeadlineSeconds`), patch `renewTime` to now via optimistic concurrency. If a renewal patch ever fails (conflict or error), treat this as an immediate loss of leadership — do not retry the same renewal.

**Pure/impure split**, matching the pattern already used throughout this codebase (`helm::build_values`, `manifests::apply_rank`, `apply::resources_to_prune`):

- A pure decision function — given the Lease's current state (holder, `renewTime`, `resourceVersion`), this replica's identity, and the current time — returns what to do next: attempt acquire, attempt renew, or wait. This is unit-testable without a cluster.
- A thin async wrapper performs the actual `get`/`create`/`update` calls and drives the loop, calling the pure function to decide each step.

## 2. Integration into the Controller

**Deliberately not** the "obvious" design of racing `Controller::run(...)` against a "lost leadership" signal via `tokio::select!` and tearing the Controller down/rebuilding it on every leadership transition. Cleanly cancelling a Rust future mid-poll does not achieve "let the in-flight reconcile finish" — it cuts the future off wherever it happens to be paused (e.g. mid network call). Achieving genuine graceful drain would require real machinery (an in-flight counter, a shutdown token threaded through render/apply/prune) that this spec explicitly avoids (see Non-goals).

Instead:

- **Every replica runs the same `Controller` loop, unconditionally, all the time** — identical to today's `main.rs` wiring (including the existing generation-based watch filter from the hot-reconcile-loop fix, which needs no changes).
- A shared `is_leader: Arc<AtomicBool>` field is added to `reconciler::Context`, written by the leader-election background task (Section 1) whenever leadership is acquired or lost.
- `reconcile()` gains one check at its very start: if `!is_leader.load(Ordering::Relaxed)`, immediately return `Ok(Action::requeue(Duration::from_secs(15)))` — a cheap no-op with no render, no apply, no status write. The 15s figure matches `leaseDurationSeconds` purely to keep standby reconciles infrequent; it has no effect on failover speed, since the leadership flag itself is updated by the independent background task from Section 1, not by this reconcile call. Only the replica that currently believes itself leader does any real work.

This achieves "hard cutover, let it finish" without any cancellation logic: a reconcile that started while leader runs to completion untouched, even if leadership flips mid-flight, because nothing ever interrupts it — the flag is only consulted at the *start* of each reconcile.

**Scope of that guarantee — lease loss only, not process shutdown.** "Let it finish" holds for *leadership* transitions, because the only enforcement point is the `is_leader` check at the start of `reconcile()`. It does **not** hold for process shutdown via SIGTERM (Section 4). `tokio::select!` drops every non-winning branch's future, and `kube-runtime`'s `Controller`/`Runner` does not spawn reconciles onto separate tasks — they are driven inline by the `Controller` future, which is itself one of the `select!` branches. So when SIGTERM wins the race, any in-flight reconcile is dropped immediately, wherever it happened to be paused. This is safe, but for a different reason than "let it finish": the successor leader's next reconcile re-applies the *full* resource set recorded in `status.appliedResources` from before the interruption, so the cluster converges regardless of how far the interrupted reconcile got.

The narrow overlap window this design permits (an old leader's last in-flight reconcile finishing while a new leader's first reconcile also runs) is bounded by the Lease's timing parameters, and is benign because **render + apply + prune is idempotent**: two leaders reconciling the same unchanged spec render identical object sets, so their server-side applies are byte-identical writes and `apply::resources_to_prune` between them is empty.

It is specifically *not* Kubernetes' optimistic concurrency that saves us here — this controller's write paths do not use it for this purpose. `apply::apply_object` uses `Patch::Apply(...).force()` with a single shared field-manager name (`"platform-controller"`) across all replicas, and force-apply under a shared manager has no cross-writer conflict detection by design; `reconciler::update_status` uses `Patch::Merge` with default `PatchParams`, carrying no `resourceVersion` precondition at all. Both replicas' writes simply succeed.

That weaker, idempotency-based argument is exactly why bounding the renewal deadline in wall-clock terms matters (Section 1's `renewDeadlineSeconds`, enforced with an absolute deadline across *every* networked lease call so a hung apiserver request cannot outlive it). Idempotency only covers two leaders holding the *same* spec. A replica wedged mid-renewal long enough for a standby to take over could still be acting on a spec revision the new leader has already moved past — and then the two leaders' applies genuinely conflict, each reverting the other. Preventing that divergence window is the safety property the deadline enforcement provides; idempotency alone would not.

## 3. Deployment Topology

Changes to `deploy/bootstrap.yaml`:

- `replicas: 1` → `replicas: 2`.
- Add `POD_NAME` env var to the container (`fieldRef: fieldPath: metadata.name`) — the Lease holder identity.
- Add `preferredDuringSchedulingIgnoredDuringExecution` pod anti-affinity (topology key `kubernetes.io/hostname`, matching the existing `app: platform-controller` pod label). *Preferred*, not required — a small dev/test cluster may only have one or two control-plane nodes, and a hard requirement could leave a replica permanently unschedulable.
- Add a new `PodDisruptionBudget` (`policy/v1`, `maxUnavailable: 1`, same label selector as the Deployment) so *voluntary disruptions* — `kubectl drain`, cluster-autoscaler node scale-down, anything else going through the Eviction API — cannot remove both replicas at once. A PDB gates **only** the Eviction API; it has no bearing on a Deployment's own rollout. Rollout safety here comes instead from the Deployment's `strategy.rollingUpdate` defaults (unset in the manifest, so Kubernetes applies 25% `maxUnavailable` and 25% `maxSurge`, which at 2 replicas round to 0 and 1 respectively — i.e. surge-then-replace, never both replicas down).
  - **Single-control-plane-node hazard:** with only one control-plane node (as in this project's own `--workers 1` Talos-in-Docker test recipe) the anti-affinity is merely *preferred*, so both replicas co-locate. `maxUnavailable: 1` then permanently blocks a `kubectl drain` of that node: the first eviction succeeds, and the second is refused forever, because evicting it would leave 0 healthy replicas against a budget requiring at least 1. Escape hatch: temporarily `kubectl delete pdb -n platform-system platform-controller`, or drain with `--disable-eviction`.

**RBAC:** no manifest changes needed. `coordination.k8s.io/leases` get/create/update is already covered by the existing `cluster-admin` binding (see [docs/memory/rbac-cluster-admin-tradeoff.md](../../memory/rbac-cluster-admin-tradeoff.md)). This permission requirement is added as a new row to that memory's permission ledger (attributed to "Controller itself," not a managed chart) as part of implementation, since that ledger exists specifically to track this for the eventual RBAC scope-down.

## 4. Observability and Graceful Shutdown

- `tracing::info!` when leadership is acquired or lost.
- `tracing::warn!` on a failed lease renewal (the trigger for losing leadership).
- `tracing::debug!` on routine successful renewals (would be noisy at `info` given the ~2-7s renewal cadence).
- **Graceful shutdown on SIGTERM:** `main.rs` currently has no signal handling at all. This spec adds a SIGTERM handler that, if this replica currently holds the Lease, best-effort-patches it to an already-expired state before the process exits. This lets a `kubectl rollout restart` or normal pod eviction hand off to a standby in roughly one `retryPeriodSeconds` (~2s) instead of waiting out the full lease timeout (~15-25s), directly serving the failover goal for the common case (voluntary disruption — deploys, drains) rather than only the rarer ungraceful-crash case where the timeout path is what fires.

## 5. Testing Strategy

- **Unit tests** (no cluster required): the pure lease-decision function, covering: no Lease exists yet (attempt create); Lease held by self (renew); Lease held by another identity and not expired (wait); Lease held by another identity and expired (attempt takeover); a renewal patch conflict while holding (treat as lost).
- **Live Talos verification**, matching this project's established bar (every risky piece of this controller so far has been proven against a real cluster, not review alone): a new `#[ignore]`d test (or documented manual procedure alongside the existing `tests/integration_talos.rs`) that scales the Deployment to 2 replicas, confirms exactly one pod holds the Lease, force-deletes that pod, and confirms — within a bounded window (lease duration + renew deadline + a small buffer, ~30s) — the other pod becomes holder and a spec change made after the kill is still reconciled successfully.
