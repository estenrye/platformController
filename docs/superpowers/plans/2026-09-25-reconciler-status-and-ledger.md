# Durable Ledger and Failure Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make both reconcilers (`CniInstallation`, `PullThroughCache`) save their applied-resources ledger *before* applying, and write a `Failed` status with a reason on any post-validation error.

**Architecture:** A new pure module `src/ledger.rs` holds the ledger arithmetic (`merge_ledger`, `checkpoint_ledger`, `failure_ledger`) and a `ReconcileProgress` value. Each reconciler's `reconcile` becomes a thin wrapper around `reconcile_inner`, which records the full `desired` list as soon as it is known and writes a checkpoint status before the first apply. The wrapper turns any non-validation error into a `Failed` status and returns the original error unchanged. Apply, prune and cleanup code are not modified.

**Tech Stack:** Rust (edition 2024), `kube` 4.2, existing `apply`/`manifests`/`helm` modules. No new dependencies, no CRD change.

**Spec:** [docs/superpowers/specs/2026-09-25-reconciler-status-and-ledger-design.md](../specs/2026-09-25-reconciler-status-and-ledger-design.md)

## Global Constraints

Copied from the spec; every task's requirements implicitly include this section.

- **Invariant:** no object is applied unless it is already in the durable ledger. If the checkpoint write fails, the reconcile returns that error before applying anything.
- Checkpoint only when `desired` contains an entry not in the current `status.appliedResources` (steady-state resyncs must write nothing extra). Checkpoint status: `phase = Installing`, condition `Applied=False`, reason `Applying`, `appliedResources = merge_ledger(previous, desired)`.
- `merge_ledger`: `desired` in apply order, then any `previous` entries not in `desired`; duplicates removed, first occurrence wins.
- Failure status: `phase = Failed`, condition `Applied=False`, message = the error text, `appliedResources = merge_ledger(previous, desired)` (or `previous` if `desired` was not yet known). The original error is returned unchanged, so `error_policy` and logging behave as before.
- Reasons: `Helm` → `RenderFailed`; `Manifest` → `InvalidManifest`; `Apply` → `ApplyFailed`. **No failure status** for `Validation` (it already writes its own), `Status` (a failed status write cannot be reported through status) or `NotLeader`.
- The final successful write is unchanged: `Ready`, `appliedResources = applied` (the ledger after apply, with pruned entries dropped).
- Cleanup, prune, the Calico sweep, `apply.rs` and `deploy/crd.yaml` are NOT modified. `deploy/crd.yaml` must stay byte-identical (the freshness test enforces it).
- Every existing CNI reconciler test must pass unchanged.
- Clippy: `result_large_err` already fires on both reconcilers and is accepted; no other new warning class.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Building needs free disk. If a build fails with `No space left on device`, stop and report; do not delete anything that is not a build artifact. Share one build directory: `export CARGO_TARGET_DIR=/Users/esten/src/platformController/target CARGO_INCREMENTAL=0`.

## Branch

Work on the existing branch `reconciler-status-ledger` (worktree `.claude/worktrees/reconciler-status-ledger`, cut from `pull-through-cache`; it already holds the spec and this plan). It depends on `src/cache_reconciler.rs` from the `pull-through-cache` branch.

## Testing honesty

`reconcile` needs a Kubernetes client and cannot be unit-tested here. What IS unit-tested: all ledger arithmetic, and the error-to-reason mapping (an exhaustive `match`, so adding an error variant later forces a decision). The wrapper and checkpoint wiring are verified by the live acceptance in Task 5.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/ledger.rs` (create) | Pure ledger functions and `ReconcileProgress`. The only code shared by the two reconcilers. |
| `src/lib.rs` (modify) | Register `ledger`. |
| `src/cache_reconciler.rs` (modify) | `failure_reason`, `reconcile` wrapper + `reconcile_inner`, checkpoint, `record_failure`. |
| `src/reconciler.rs` (modify) | Same three changes for the CNI reconciler. |
| docs, memory, example (modify) | Remove the "known gap" caveats. |

---

## Task 1: The pure ledger module

**Files:**
- Create: `src/ledger.rs`
- Modify: `src/lib.rs` (add `pub mod ledger;` between `pub mod leader;` and `pub mod manifests;`)

**Interfaces:**
- Consumes: `crate::crd::AppliedResourceRef` (derives `Clone, PartialEq, Eq, Hash`).
- Produces (all `pub` in `crate::ledger`):
  - `fn merge_ledger(previous: &[AppliedResourceRef], desired: &[AppliedResourceRef]) -> Vec<AppliedResourceRef>`
  - `fn ledger_needs_checkpoint(previous: &[AppliedResourceRef], desired: &[AppliedResourceRef]) -> bool`
  - `fn checkpoint_ledger(previous: &[AppliedResourceRef], desired: &[AppliedResourceRef]) -> Option<Vec<AppliedResourceRef>>`
  - `fn failure_ledger(previous: &[AppliedResourceRef], desired: Option<&[AppliedResourceRef]>) -> Vec<AppliedResourceRef>`
  - `struct ReconcileProgress { pub desired: Option<Vec<AppliedResourceRef>> }` (`Default`, `Debug`)

- [ ] **Step 1: Register the module and write the failing tests**

In `src/lib.rs` add `pub mod ledger;` (alphabetical: after `leader`, before `manifests`). Create `src/ledger.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn resource(kind: &str, name: &str) -> AppliedResourceRef {
        AppliedResourceRef {
            api_version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn merge_keeps_desired_in_apply_order_then_stale_previous_entries() {
        let previous = vec![resource("Service", "old"), resource("Namespace", "ns")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert_eq!(
            merge_ledger(&previous, &desired),
            vec![
                resource("Namespace", "ns"),
                resource("DaemonSet", "ds"),
                resource("Service", "old"),
            ]
        );
    }

    #[test]
    fn merge_removes_duplicates_first_occurrence_wins() {
        let desired = vec![
            resource("Namespace", "ns"),
            resource("DaemonSet", "ds"),
            resource("Namespace", "ns"),
        ];

        assert_eq!(
            merge_ledger(&[], &desired),
            vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")]
        );
    }

    #[test]
    fn merge_of_empty_inputs_is_empty() {
        assert!(merge_ledger(&[], &[]).is_empty());
    }

    #[test]
    fn a_first_install_needs_a_checkpoint() {
        let desired = vec![resource("Namespace", "ns")];

        assert!(ledger_needs_checkpoint(&[], &desired));
    }

    #[test]
    fn an_identical_ledger_needs_no_checkpoint() {
        let ledger = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert!(!ledger_needs_checkpoint(&ledger, &ledger));
    }

    #[test]
    fn a_reordered_ledger_needs_no_checkpoint() {
        let previous = vec![resource("DaemonSet", "ds"), resource("Namespace", "ns")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert!(!ledger_needs_checkpoint(&previous, &desired));
    }

    #[test]
    fn a_new_object_needs_a_checkpoint() {
        let previous = vec![resource("Namespace", "ns")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert!(ledger_needs_checkpoint(&previous, &desired));
    }

    #[test]
    fn dropping_an_object_alone_needs_no_checkpoint() {
        // Pruning is handled after applying, against the previous ledger; a
        // checkpoint only protects objects that are about to be created.
        let previous = vec![resource("Namespace", "ns"), resource("Service", "old")];
        let desired = vec![resource("Namespace", "ns")];

        assert!(!ledger_needs_checkpoint(&previous, &desired));
    }

    #[test]
    fn checkpoint_is_none_in_steady_state_and_the_merged_ledger_otherwise() {
        let previous = vec![resource("Namespace", "ns")];
        let grown = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert_eq!(checkpoint_ledger(&previous, &previous), None);
        assert_eq!(
            checkpoint_ledger(&previous, &grown),
            Some(vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")])
        );
    }

    #[test]
    fn a_failure_before_desired_is_known_keeps_the_previous_ledger() {
        let previous = vec![resource("Namespace", "ns")];

        assert_eq!(failure_ledger(&previous, None), previous);
    }

    #[test]
    fn a_failure_after_desired_is_known_covers_everything_that_may_exist() {
        let previous = vec![resource("Service", "old")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert_eq!(
            failure_ledger(&previous, Some(&desired)),
            vec![
                resource("Namespace", "ns"),
                resource("DaemonSet", "ds"),
                resource("Service", "old"),
            ]
        );
    }

    #[test]
    fn progress_starts_with_no_desired_list() {
        assert_eq!(ReconcileProgress::default().desired, None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib ledger`
Expected: compile errors, `cannot find function \`merge_ledger\`` (and the other names) and `cannot find type \`AppliedResourceRef\``.

- [ ] **Step 3: Write the implementation**

Insert at the top of `src/ledger.rs`, above the `#[cfg(test)]` block:

```rust
use crate::crd::AppliedResourceRef;

/// What a reconcile is about to apply, recorded as soon as it is known so a
/// failure can still persist a ledger covering everything that may exist.
#[derive(Debug, Default)]
pub struct ReconcileProgress {
    pub desired: Option<Vec<AppliedResourceRef>>,
}

/// `desired` in apply order, followed by any `previous` entries not in
/// `desired`. Duplicates are removed; the first occurrence wins.
///
/// Order matters: CNI cleanup deletes in reverse ledger order (and moves the
/// operator's `Installation` last), so apply order must be preserved. Stale
/// entries go last so cleanup, which reverses, removes them first.
pub fn merge_ledger(
    previous: &[AppliedResourceRef],
    desired: &[AppliedResourceRef],
) -> Vec<AppliedResourceRef> {
    let mut merged: Vec<AppliedResourceRef> = Vec::with_capacity(previous.len().max(desired.len()));
    for reference in desired.iter().chain(previous.iter()) {
        if !merged.contains(reference) {
            merged.push(reference.clone());
        }
    }
    merged
}

/// True when `desired` holds an entry missing from `previous`, i.e. the
/// reconcile is about to create something the saved ledger does not know about.
pub fn ledger_needs_checkpoint(
    previous: &[AppliedResourceRef],
    desired: &[AppliedResourceRef],
) -> bool {
    desired.iter().any(|reference| !previous.contains(reference))
}

/// The ledger to persist before applying, or `None` in steady state, so a
/// periodic resync writes nothing extra.
pub fn checkpoint_ledger(
    previous: &[AppliedResourceRef],
    desired: &[AppliedResourceRef],
) -> Option<Vec<AppliedResourceRef>> {
    ledger_needs_checkpoint(previous, desired).then(|| merge_ledger(previous, desired))
}

/// The ledger to persist when a reconcile fails: everything that may exist.
pub fn failure_ledger(
    previous: &[AppliedResourceRef],
    desired: Option<&[AppliedResourceRef]>,
) -> Vec<AppliedResourceRef> {
    match desired {
        Some(desired) => merge_ledger(previous, desired),
        None => previous.to_vec(),
    }
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib ledger`
Expected: PASS (12 tests).

- [ ] **Step 5: Commit**

```bash
git add src/ledger.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: pure ledger arithmetic for durable checkpoints and failure status

merge_ledger, checkpoint_ledger and failure_ledger, plus ReconcileProgress.
Shared by both reconcilers in the following commits.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `PullThroughCache` reconciler: checkpoint and failure status

**Files:**
- Modify: `src/cache_reconciler.rs`

**Interfaces:**
- Consumes: `crate::ledger::{ReconcileProgress, checkpoint_ledger, failure_ledger}`; existing `update_status(api, name, phase, generation, chart_version, applied_resources, reason, message)` in the same file; `crate::apply::resource_ref`.
- Produces: `CacheReconcileError::failure_reason(&self) -> Option<&'static str>`; `reconcile` keeps its signature `(Arc<PullThroughCache>, Arc<Context>) -> Result<Action, CacheReconcileError>` (now a wrapper), so `reconcile_with_finalizer` and `main.rs` are untouched.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/cache_reconciler.rs`:

```rust
    #[test]
    fn helm_errors_report_render_failed() {
        let err = CacheReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = CacheReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn apply_errors_report_apply_failed() {
        let err = CacheReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "Namespace".to_string(),
            name: "spegel".to_string(),
            timeout: std::time::Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn validation_and_leadership_errors_do_not_overwrite_status() {
        // Validation already writes its own Failed status; a standby must never
        // write status at all.
        let validation = CacheReconcileError::Validation(ValidationError::UnsupportedName(
            "second".to_string(),
        ));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(CacheReconcileError::NotLeader.failure_reason(), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cache_reconciler`
Expected: compile error, `no method named \`failure_reason\` found for enum \`CacheReconcileError\``.

- [ ] **Step 3: Add `failure_reason`**

Directly below the `CacheReconcileError` enum definition in `src/cache_reconciler.rs`, add:

```rust
impl CacheReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            CacheReconcileError::Helm(_) => Some("RenderFailed"),
            CacheReconcileError::Manifest(_) => Some("InvalidManifest"),
            CacheReconcileError::Apply(_) => Some("ApplyFailed"),
            CacheReconcileError::Validation(_)
            | CacheReconcileError::Status(_)
            | CacheReconcileError::NotLeader => None,
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cache_reconciler`
Expected: PASS (the 5 existing tests plus the 4 new ones).

- [ ] **Step 5: Turn `reconcile` into a wrapper around `reconcile_inner`**

In `src/cache_reconciler.rs` replace the function header

```rust
pub async fn reconcile(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, CacheReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
```

with (the body of the old `reconcile` is unchanged and now belongs to `reconcile_inner`):

```rust
/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
) -> Result<Action, CacheReconcileError> {
    let mut progress = crate::ledger::ReconcileProgress::default();
    let result = reconcile_inner(obj.clone(), ctx.clone(), &mut progress).await;
    if let Err(err) = &result
        && let Some(reason) = err.failure_reason()
    {
        record_failure(&obj, &ctx, &progress, reason, &err.to_string()).await;
    }
    result
}

/// Best effort: a failed status write is logged and never replaces the
/// reconcile's own error.
async fn record_failure(
    obj: &PullThroughCache,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<PullThroughCache> = kube::Api::all(ctx.client.clone());
    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();
    let ledger = crate::ledger::failure_ledger(&previous, progress.desired.as_deref());

    if let Err(err) = update_status(
        &api,
        &name,
        Phase::Failed,
        obj.metadata.generation,
        &obj.spec.spegel.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(cache = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<PullThroughCache>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, CacheReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
```

- [ ] **Step 6: Add the checkpoint before the first apply**

In `reconcile_inner`, immediately after the statement

```rust
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );
```

and before `let mut applied = Vec::new();`, insert:

```rust
    // Everything this reconcile will apply is known now. Persist it before the
    // first apply so a failure, a crash or a leader change can never leave an
    // applied object out of the ledger cleanup acts on. Steady-state resyncs
    // add nothing, so they write nothing.
    let mut desired = vec![crate::apply::resource_ref(&spegel_namespace_object())];
    desired.extend(objects.iter().map(crate::apply::resource_ref));
    progress.desired = Some(desired.clone());
    if let Some(ledger) = crate::ledger::checkpoint_ledger(&previous, &desired) {
        tracing::info!(entries = ledger.len(), "checkpointing ledger before applying");
        update_status(
            &api,
            &name,
            Phase::Installing,
            obj.metadata.generation,
            &chart_version,
            &ledger,
            "Applying",
            "applying manifests",
        )
        .await?;
    }

```

- [ ] **Step 7: Guard the invariant in debug builds**

In `reconcile_inner`, immediately before the line `tracing::info!(cache = %name, phase = ?Phase::Ready, "updating status");`, insert:

```rust
    debug_assert!(
        applied.iter().all(|reference| desired.contains(reference)),
        "applied an object that was not in the checkpointed ledger"
    );
```

- [ ] **Step 8: Build, test and lint**

Run: `cargo build && cargo test --lib cache_reconciler && cargo clippy --all-targets 2>&1 | grep -E "^warning: " | sort | uniq -c`
Expected: builds; tests PASS; clippy shows only classes already present on this branch (`result_large_err`, `too_many_arguments`, `unnecessary use of clone`, `&mut Vec`); in particular no `unused` or `needless` warnings from the new code.

- [ ] **Step 9: Commit**

```bash
git add src/cache_reconciler.rs
git commit -m "$(cat <<'EOF'
feat: PullThroughCache checkpoints its ledger and reports failures in status

reconcile is now a wrapper: reconcile_inner records the desired ledger and
writes an Installing checkpoint before the first apply when the ledger would
grow; any render, manifest or apply error is written as Failed with a reason
and the merged ledger before the original error is returned.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `CniInstallation` reconciler: the same change

**Files:**
- Modify: `src/reconciler.rs`

**Interfaces:**
- Consumes: as Task 2; the existing `update_status` in `src/reconciler.rs` has the same parameter list; `tigera_operator_namespace_object()`, `crate::calico::{pod_pool_objects, routing_and_lb_objects}`.
- Produces: `ReconcileError::failure_reason(&self) -> Option<&'static str>`; `reconcile` keeps its signature (now a wrapper).

**Care:** this is the live-verified CNI path. Do not edit any apply, prune, cleanup, sweep or ordering code, and do not move existing statements. Only the three insertions below.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/reconciler.rs`:

```rust
    #[test]
    fn cni_helm_errors_report_render_failed() {
        let err = ReconcileError::Helm(crate::helm::HelmError::WriteValues(
            std::io::Error::other("boom"),
        ));

        assert_eq!(err.failure_reason(), Some("RenderFailed"));
    }

    #[test]
    fn cni_manifest_errors_report_invalid_manifest() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = ReconcileError::Manifest(crate::manifests::ManifestError::Json {
            index: 0,
            source,
        });

        assert_eq!(err.failure_reason(), Some("InvalidManifest"));
    }

    #[test]
    fn cni_apply_errors_report_apply_failed() {
        let err = ReconcileError::Apply(crate::apply::ApplyError::NotDeleted {
            kind: "Installation".to_string(),
            name: "default".to_string(),
            timeout: Duration::from_secs(1),
        });

        assert_eq!(err.failure_reason(), Some("ApplyFailed"));
    }

    #[test]
    fn cni_validation_and_leadership_errors_do_not_overwrite_status() {
        let validation =
            ReconcileError::Validation(ValidationError::UnsupportedName("second".to_string()));

        assert_eq!(validation.failure_reason(), None);
        assert_eq!(ReconcileError::NotLeader.failure_reason(), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib reconciler::tests::cni_`
Expected: compile error, `no method named \`failure_reason\` found for enum \`ReconcileError\``.

- [ ] **Step 3: Add `failure_reason`**

Directly below the `ReconcileError` enum definition in `src/reconciler.rs`, add:

```rust
impl ReconcileError {
    /// The `status.conditions[].reason` to report for a failed reconcile, or
    /// `None` when this error must not overwrite status: `Validation` already
    /// wrote its own, a failed `Status` write cannot be reported through
    /// status, and a standby (`NotLeader`) must never write status.
    ///
    /// Exhaustive on purpose: a new variant forces a decision here.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            ReconcileError::Helm(_) => Some("RenderFailed"),
            ReconcileError::Manifest(_) => Some("InvalidManifest"),
            ReconcileError::Apply(_) => Some("ApplyFailed"),
            ReconcileError::Validation(_) | ReconcileError::Status(_) | ReconcileError::NotLeader => {
                None
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib reconciler`
Expected: PASS: every pre-existing reconciler test unchanged, plus the 4 new ones.

- [ ] **Step 5: Turn `reconcile` into a wrapper around `reconcile_inner`**

In `src/reconciler.rs` replace the single header line

```rust
pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
```

with (the old body is unchanged and now belongs to `reconcile_inner`):

```rust
/// Runs one reconcile. A failure after validation is written to `status` (with
/// a reason and the ledger of everything that may exist) before the original
/// error is returned, so `error_policy` and the log behave as before.
pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let mut progress = crate::ledger::ReconcileProgress::default();
    let result = reconcile_inner(obj.clone(), ctx.clone(), &mut progress).await;
    if let Err(err) = &result
        && let Some(reason) = err.failure_reason()
    {
        record_failure(&obj, &ctx, &progress, reason, &err.to_string()).await;
    }
    result
}

/// Best effort: a failed status write is logged and never replaces the
/// reconcile's own error.
async fn record_failure(
    obj: &CniInstallation,
    ctx: &Context,
    progress: &crate::ledger::ReconcileProgress,
    reason: &str,
    message: &str,
) {
    let name = obj.name_any();
    let api: kube::Api<CniInstallation> = kube::Api::all(ctx.client.clone());
    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();
    let ledger = crate::ledger::failure_ledger(&previous, progress.desired.as_deref());

    if let Err(err) = update_status(
        &api,
        &name,
        Phase::Failed,
        obj.metadata.generation,
        &obj.spec.calico.chart_version,
        &ledger,
        reason,
        message,
    )
    .await
    {
        tracing::warn!(installation = %name, error = %err, "failed to record failure status");
    }
}

async fn reconcile_inner(
    obj: Arc<CniInstallation>,
    ctx: Arc<Context>,
    progress: &mut crate::ledger::ReconcileProgress,
) -> Result<Action, ReconcileError> {
```

- [ ] **Step 6: Add the checkpoint before the first apply**

In `reconcile_inner`, immediately after the statement

```rust
    tracing::info!(
        object_count = objects.len(),
        "parsed and sorted rendered manifests"
    );
```

and before `let mut applied = Vec::new();`, insert. The two Calico phase functions are pure functions of the spec, so calling them here as well as at their existing use is deliberate and leaves the existing code untouched:

```rust
    // Everything this reconcile will apply is known now, in apply order:
    // the target namespace, the chart's objects, then Calico's own two phases.
    // Persist it before the first apply so a failure, a crash or a leader
    // change can never leave an applied object out of the ledger cleanup acts
    // on. Steady-state resyncs add nothing, so they write nothing.
    let mut desired = vec![crate::apply::resource_ref(&tigera_operator_namespace_object())];
    desired.extend(objects.iter().map(crate::apply::resource_ref));
    desired.extend(
        crate::calico::pod_pool_objects(&obj.spec.calico)
            .iter()
            .map(crate::apply::resource_ref),
    );
    desired.extend(
        crate::calico::routing_and_lb_objects(&obj.spec.calico)
            .iter()
            .map(crate::apply::resource_ref),
    );
    progress.desired = Some(desired.clone());
    if let Some(ledger) = crate::ledger::checkpoint_ledger(&previous, &desired) {
        tracing::info!(entries = ledger.len(), "checkpointing ledger before applying");
        update_status(
            &api,
            &name,
            Phase::Installing,
            obj.metadata.generation,
            &chart_version,
            &ledger,
            "Applying",
            "applying manifests",
        )
        .await?;
    }

```

- [ ] **Step 7: Guard the invariant in debug builds**

In `reconcile_inner`, immediately before the line `tracing::info!(installation = %name, phase = ?Phase::Ready, "updating status");`, insert:

```rust
    debug_assert!(
        applied.iter().all(|reference| desired.contains(reference)),
        "applied an object that was not in the checkpointed ledger"
    );
```

- [ ] **Step 8: Build, test and lint**

Run: `cargo build && cargo test && cargo clippy --all-targets 2>&1 | grep -E "^warning: " | sort | uniq -c`
Expected: builds; the whole suite PASSES (including `tests/bootstrap_manifests.rs`, which proves `deploy/crd.yaml` is unchanged); clippy shows only warning classes already present on this branch.

- [ ] **Step 9: Commit**

```bash
git add src/reconciler.rs
git commit -m "$(cat <<'EOF'
feat: CniInstallation checkpoints its ledger and reports failures in status

Same wrapper/checkpoint/failure-status change as PullThroughCache. The apply,
prune, cleanup and Calico sweep code is untouched; the desired list is the
target namespace, the chart objects and Calico's two phases, in apply order.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Remove the "known gap" caveats from docs, memory and the example

**Files:**
- Modify: `docs/runbooks/pull-through-cache-verification.md`
- Modify: `docs/superpowers/specs/2026-09-25-pull-through-cache-design.md`
- Modify: `docs/memory/ipv6-only-calico-2026-09.md`
- Modify: `docs/memory/pull-through-cache-2026-09.md`
- Modify: `examples/pull-through-cache.yaml`

**Interfaces:** none (documentation). Locate each passage with the `grep` given; if a passage does not match, stop and report NEEDS_CONTEXT rather than guessing.

- [ ] **Step 1: Runbook**

`grep -n "When something goes wrong" docs/runbooks/pull-through-cache-verification.md` finds a section. Replace that whole section (from its `##` heading up to, not including, the next `##` heading) with:

```markdown
## When something goes wrong

Failures appear on the resource, not only in the controller's logs:

- `kubectl get ptc default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidRegistry`, `InvalidChartVersion` and the
  other validation reasons; `RenderFailed` when helm cannot render the chart,
  e.g. a `v` prefix on the chart tag; `InvalidManifest`; `ApplyFailed` when the
  API server or a wait rejects an object) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is applied, so
  deleting the resource after a failed first install still removes everything
  that was created, including the `spegel` namespace. No manual
  `kubectl delete ns spegel` is needed.
- A resource that was `Ready` and hits a transient failure shows `Failed` until
  the next successful reconcile (retried every 30 seconds).
```

- [ ] **Step 2: PullThroughCache spec**

`grep -n -i "ledger\|non-validation\|only in the controller" docs/superpowers/specs/2026-09-25-pull-through-cache-design.md` finds the passages that describe the two known gaps (failures not in status; ledger written only after a full success). Replace those sentences with the single sentence: `Failures after validation are written to status and the ledger is saved before applying: see docs/superpowers/specs/2026-09-25-reconciler-status-and-ledger-design.md.` Do not touch the rest of the spec.

- [ ] **Step 2b: The `Ready` note in the spec's Cleanup section**

If the same file says a first-install failure followed by delete leaves the namespace behind, reword it to point at the same spec (one sentence).

- [ ] **Step 3: Memory files**

In `docs/memory/ipv6-only-calico-2026-09.md`, find the sentence beginning `` `status.appliedResources` is only written after every phase succeeds (pre-existing) `` and ending `remove them by hand.` Replace exactly that sentence with: `` `status.appliedResources` is now checkpointed before applying (see docs/superpowers/specs/2026-09-25-reconciler-status-and-ledger-design.md), so a first install that fails partway and is then deleted is cleaned up; before that fix the ledger was empty and the `tigera-operator` namespace, RBAC and Deployment were left behind. ``

In `docs/memory/pull-through-cache-2026-09.md`, `grep -n "follow-up" docs/memory/pull-through-cache-2026-09.md` finds the bullet about non-validation failures not writing status and an empty first-install ledger. Replace that bullet with: `Failures after validation now write status (Failed with RenderFailed / InvalidManifest / ApplyFailed) and the ledger is checkpointed before applying; see docs/superpowers/specs/2026-09-25-reconciler-status-and-ledger-design.md.`

- [ ] **Step 4: Example comment**

In `examples/pull-through-cache.yaml`, `grep -n "controller logs" examples/pull-through-cache.yaml` finds the comment near `chartVersion`. Reword it to: `# If .status shows Failed / RenderFailed, the usual cause is a "v" prefix on the chart tag.` (keep it a single comment line at the same position).

- [ ] **Step 5: Verify nothing else broke**

Run: `cargo test --test pull_through_cache_example --test bootstrap_manifests`
Expected: PASS (the example test parses the edited YAML).

- [ ] **Step 6: Commit**

```bash
git add docs examples
git commit -m "$(cat <<'EOF'
docs: the ledger is now durable and failures appear in status

Remove the known-gap caveats from the runbook, both specs' notes, the memory
files and the example comment.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: MANUAL: live acceptance on a real cluster

Requires a Talos cluster reachable through a kubeconfig and a person or agent allowed to change it. It cannot be delegated blindly: it applies and deletes resources on that cluster. The code is exercised by running the branch controller **locally** against the cluster (the nodes cannot pull an unpublished image).

**Setup**

```bash
export CARGO_TARGET_DIR=/Users/esten/src/platformController/target CARGO_INCREMENTAL=0
export KUBECONFIG=$HOME/.kube/kubeconfig
cargo build
kubectl apply -f deploy/crd.yaml            # unchanged by this work; harmless
kubectl get ns platform-system || kubectl create namespace platform-system
RUST_LOG=info POD_NAME=local-controller $CARGO_TARGET_DIR/debug/platform-controller > /tmp/live-controller.log 2>&1 &
echo $! > /tmp/live-controller.pid
```

Requires the node to have a working CNI first (a `CniInstallation` at `Ready`), because the Spegel scenarios below need pod networking.

- [ ] **A. CNI regression (existing installation, steady state)**

If a `CniInstallation` `default` already exists, expect the new controller to reconcile it without any checkpoint write: `kubectl get cni default -o jsonpath='{.status.phase}'` stays `Ready`; the log contains no `checkpointing ledger` line for it; `kubectl get ippools.crd.projectcalico.org` still lists exactly the declared pools.

- [ ] **B. Render failure is visible (PullThroughCache)**

```bash
sed 's/chartVersion: "0.7.4"/chartVersion: "v0.7.4"/' examples/pull-through-cache.yaml | kubectl apply -f -
sleep 15
kubectl get ptc default -o jsonpath='{.status.phase}{" "}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'
```

Expected: `Failed RenderFailed` and a message naming the helm "not found" error, within one reconcile (previously the status was empty). Then `kubectl patch ptc default --type merge -p '{"spec":{"spegel":{"chartVersion":"0.7.4"}}}'` and expect `Ready` within ~15 seconds.

- [ ] **C. Partial apply, then delete**

```bash
kubectl delete ptc default
sed 's/^    registries:/    helmValues:\n      resources:\n        limits:\n          memory: bogus\n    registries:/' examples/pull-through-cache.yaml | kubectl apply -f -
sleep 20
kubectl get ptc default -o jsonpath='{.status.phase}{" "}{.status.conditions[0].reason}{"\n"}{range .status.appliedResources[*]}{.kind}/{.name} {end}{"\n"}'
kubectl get ns spegel                       # exists: the namespace and some objects were applied
kubectl delete ptc default
sleep 15
kubectl get ns spegel                       # expect NotFound (before this change it was left behind)
```

Expected: `Failed ApplyFailed` with a ledger listing `Namespace/spegel`, the ServiceAccount, the Services and the DaemonSet (the API server rejects the DaemonSet's `bogus` memory quantity after the earlier objects were applied); after the delete, the `spegel` namespace is gone.

- [ ] **D. CNI failure is visible and non-destructive**

Only if the CNI can safely show a brief `Failed`: `kubectl patch cni default --type merge -p '{"spec":{"calico":{"chartVersion":"v9.9.9"}}}'`, expect `Failed RenderFailed` within one reconcile and every Calico pod still `Running` (render fails before anything is applied or pruned); then patch it back to the original `chartVersion` and expect `Ready`. If the cluster's CNI must not be touched, skip this scenario and say so.

- [ ] **Teardown**

```bash
kill -TERM $(cat /tmp/live-controller.pid)
kubectl get ptc 2>&1 | head -1; kubectl get ns spegel 2>&1 | head -1
```

- [ ] **Record the outcome**

Write what was observed (each scenario pass/fail, the exact `reason` strings, anything surprising) into `docs/memory/pull-through-cache-2026-09.md` under a new "Ledger and failure-status follow-up, live-verified" paragraph, commit as `docs: record live acceptance of the ledger and failure-status change`. If scenario B or C fails, stop: the invariant is not met and the design needs revisiting.

---

## Self-Review

**Spec coverage.** Invariant and checkpoint-before-apply (Tasks 2, 3 Step 6); `merge_ledger` and its helpers (Task 1); failure status with the reason table and the three no-write cases (Tasks 2, 3 `failure_reason` and wrapper); the steady-state no-churn rule (`checkpoint_ledger` returns `None`, Task 1 tests); cleanup unchanged and `deploy/crd.yaml` unchanged (Global Constraints, Task 3 Step 8); documentation updates (Task 4); unit tests for ledger arithmetic and reason mapping (Tasks 1-3); live acceptance scenarios 1-3 from the spec (Task 5 B, C, A) plus a non-destructive CNI failure check (Task 5 D). Decisions 1-3 (both reconcilers, flip to `Failed`, reason vocabulary) are implemented as written.

**Placeholder scan.** None. Task 4 locates prose by `grep` because its exact wording was written by earlier implementers; each step names the replacement text in full and says to stop if a passage is not found.

**Type consistency.** `ReconcileProgress.desired: Option<Vec<AppliedResourceRef>>` is set in Tasks 2/3 and read by `failure_ledger(&previous, progress.desired.as_deref())`, whose second parameter is `Option<&[AppliedResourceRef]>`. `checkpoint_ledger(&previous, &desired)` takes slices; `desired: Vec` derefs. `failure_reason` is defined on each error enum and used only by the wrapper in its own file. `update_status`'s parameter order `(api, name, phase, generation, chart_version, applied_resources, reason, message)` is identical in both reconcilers and in the calls added here.

**Verification status.** The code in Tasks 1-3 was applied to a scratch copy of this branch while planning: `cargo test` passed (144 lib unit tests = the existing 124 + 12 ledger + 4 + 4 reason-mapping, and every integration-test file), `cargo clippy --all-targets` showed only warning classes already present (`result_large_err` grew from 15 to 17 because of the two new private async functions, which is the accepted class), and `deploy/crd.yaml` was byte-identical. **Known unverified points.** The wrapper and checkpoint wiring compile and pass the unit tests but are not exercised against a cluster until Task 5. The constructors used in the `failure_reason` tests (`HelmError::WriteValues`, `ManifestError::Json`, `ApplyError::NotDeleted`) match the current definitions; a compile error there is a plan bug to fix, not to work around. The `Status` error variant is not constructed in a test (it is covered by the exhaustive `match`).
