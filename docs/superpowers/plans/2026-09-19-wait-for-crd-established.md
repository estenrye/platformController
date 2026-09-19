# Wait for CRD Established Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the reconciler from applying objects that depend on a CustomResourceDefinition before that CRD's REST endpoint is actually registered and queryable.

**Architecture:** A new pure helper (`is_established`) decides whether a `CustomResourceDefinition` value reports `Established: True`; a thin networked wrapper (`wait_for_crd_established`) polls for that using the same client/error patterns already used throughout `src/apply.rs`. `reconciler.rs`'s existing apply loop calls it immediately after applying any object whose kind is `CustomResourceDefinition`, before moving on to the next object in the sorted list.

**Tech Stack:** `k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition` (already available via the existing `k8s-openapi` dependency with the `latest` feature — no new dependency). `tokio::time::{sleep, Instant}`, already used identically in `src/leader.rs`.

**Spec:** [docs/superpowers/specs/2026-09-19-wait-for-crd-established-design.md](../specs/2026-09-19-wait-for-crd-established-design.md)

## Global Constraints

- Only waits on `CustomResourceDefinition` objects — no change to `apply_rank`'s sort order in `src/manifests.rs`, no waiting on any other kind.
- Timeout: 10 seconds. Poll interval: 200ms between checks.
- The check is generic (`kind == "CustomResourceDefinition"`), not hardcoded to `operator.tigera.io`'s specific CRDs — it must catch the same race for any CRD the rendered chart includes.

---

## Task 1: `is_established` + `wait_for_crd_established` in `src/apply.rs`

**Files:**
- Modify: `src/apply.rs`

**Interfaces:**
- Produces: `platform_controller::apply::wait_for_crd_established(client: &kube::Client, name: &str, timeout: std::time::Duration) -> Result<(), ApplyError>`, and a new `ApplyError::NotEstablished { name: String, timeout: std::time::Duration }` variant. Used by Task 2's `reconciler.rs`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/apply.rs` (find it via `grep -n "mod tests" src/apply.rs`):

```rust
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
        CustomResourceDefinition, CustomResourceDefinitionCondition, CustomResourceDefinitionStatus,
    };

    fn crd_with_conditions(conditions: Vec<CustomResourceDefinitionCondition>) -> CustomResourceDefinition {
        CustomResourceDefinition {
            status: Some(CustomResourceDefinitionStatus {
                conditions: Some(conditions),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn condition(type_: &str, status: &str) -> CustomResourceDefinitionCondition {
        CustomResourceDefinitionCondition {
            type_: type_.to_string(),
            status: status.to_string(),
            last_transition_time: None,
            message: None,
            observed_generation: None,
            reason: None,
        }
    }

    #[test]
    fn no_status_is_not_established() {
        let crd = CustomResourceDefinition::default();
        assert!(!is_established(&crd));
    }

    #[test]
    fn no_conditions_is_not_established() {
        let crd = crd_with_conditions(vec![]);
        assert!(!is_established(&crd));
    }

    #[test]
    fn established_condition_with_false_status_is_not_established() {
        let crd = crd_with_conditions(vec![condition("Established", "False")]);
        assert!(!is_established(&crd));
    }

    #[test]
    fn established_condition_with_true_status_is_established() {
        let crd = crd_with_conditions(vec![condition("Established", "True")]);
        assert!(is_established(&crd));
    }

    #[test]
    fn established_true_alongside_other_conditions_is_established() {
        let crd = crd_with_conditions(vec![
            condition("NamesAccepted", "True"),
            condition("Established", "True"),
        ]);
        assert!(is_established(&crd));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib apply:: 2>&1 | head -30`
Expected: compile error — `is_established` not defined.

- [ ] **Step 3: Implement above the test module**

Add near the top of `src/apply.rs` (alongside the existing `use` statements) and after the existing `ApplyError` enum:

```rust
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use std::time::Duration;
```

Extend the existing `ApplyError` enum with a new variant:

```rust
    #[error("CRD {name} did not become Established within {timeout:?}")]
    NotEstablished { name: String, timeout: Duration },
```

Add the pure helper and the networked wrapper (a good place is right after `resource_ref`, before `group_version_kind`):

```rust
fn is_established(crd: &CustomResourceDefinition) -> bool {
    crd.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "Established" && condition.status == "True")
        })
        .unwrap_or(false)
}

pub async fn wait_for_crd_established(
    client: &kube::Client,
    name: &str,
    timeout: Duration,
) -> Result<(), ApplyError> {
    let api: kube::Api<CustomResourceDefinition> = kube::Api::all(client.clone());
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Ok(crd) = api.get(name).await {
            if is_established(&crd) {
                return Ok(());
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ApplyError::NotEstablished {
                name: name.to_string(),
                timeout,
            });
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib apply:: 2>&1 | tail -20`
Expected: all `apply::` tests pass, including the 5 new ones.

- [ ] **Step 5: Commit**

```bash
git add src/apply.rs
git commit -m "feat: add wait_for_crd_established to close a CRD-registration race"
```

---

## Task 2: Call `wait_for_crd_established` after applying a CRD in the reconcile loop

**Files:**
- Modify: `src/reconciler.rs`

**Interfaces:**
- Consumes: `platform_controller::apply::wait_for_crd_established` (Task 1).

- [ ] **Step 1: Locate the apply loop**

Run: `grep -n "for object in &objects" src/reconciler.rs`
This is the loop to modify — currently:

```rust
    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        applied.push(reference);
    }
```

- [ ] **Step 2: Add the wait, gated on the applied object's kind**

Change it to:

```rust
    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        if reference.kind == "CustomResourceDefinition" {
            crate::apply::wait_for_crd_established(
                &ctx.client,
                &reference.name,
                std::time::Duration::from_secs(10),
            )
            .await?;
            tracing::debug!(crd = %reference.name, "CRD established");
        }
        applied.push(reference);
    }
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build 2>&1 | tail -20`
Expected: builds cleanly. `ReconcileError` already has an `Apply(#[from] crate::apply::ApplyError)` variant (check with `grep -n "ApplyError" src/reconciler.rs` if unsure), so the new `ApplyError::NotEstablished` variant from Task 1 automatically propagates through the existing `?` in this function with no further changes needed.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test 2>&1 | tail -40`
Expected: all previously-passing tests still pass; this task adds no new unit tests of its own (the change is a straightforward call to Task 1's already-tested function, gated on a field comparison — the acceptance bar here is a clean build and no regressions, matching this codebase's established pattern for reconciler glue code).

- [ ] **Step 5: Commit**

```bash
git add src/reconciler.rs
git commit -m "feat: wait for CRD Established before applying objects that depend on it"
```

- [ ] **Step 6: Live-verify against a real cluster starting from a clean state**

This is the step that actually proves the fix — the unit tests only confirm the `Established`-detection logic, not that the race is closed end-to-end.

1. Find or create a cluster where the `operator.tigera.io` CRDs do NOT already exist (e.g. delete them first if testing against a cluster that already has them: `kubectl delete crd installations.operator.tigera.io apiservers.operator.tigera.io imagesets.operator.tigera.io tigerastatuses.operator.tigera.io tigerastatuses.operator.tigera.io 2>/dev/null` — only do this on a test/throwaway cluster, never on infrastructure someone else depends on).
2. Deploy the fixed controller image and apply a fresh `CniInstallation` (see `deploy/README.md` for the apply order, and `tests/integration_talos.rs`'s header comment for the local-registry-mirror approach if the target cluster can't reach Docker Hub).
3. Watch the controller's logs from the very first reconcile: `kubectl logs -n platform-system -l app=platform-controller -f`.
4. Confirm there is NO `ERROR ... Missing Kind` log line, and `status.phase` reaches `Ready` on the first reconcile attempt (not after a 30-second retry).
5. Record the actual outcome — do not claim success without having watched this happen.

---

## Self-Review Notes

- **Spec coverage:** the spec's Design section (new function + integration point) maps to Task 1 and Task 2 respectively. The spec's Testing section's unit-test item maps to Task 1 Step 1; the live-verification item maps to Task 2 Step 6.
- **Placeholder scan:** no TBD/TODO; the live-verification step gives concrete commands and an explicit "record the actual outcome" instruction.
- **Type consistency:** `wait_for_crd_established`'s signature (`&kube::Client, &str, std::time::Duration`) and `ApplyError::NotEstablished`'s fields are used identically between Task 1 (definition) and Task 2 (call site). `AppliedResourceRef.kind`/`AppliedResourceRef.name` (used in Task 2's `reference.kind`/`reference.name`) are pre-existing fields on the already-defined `AppliedResourceRef` type — verified against `src/crd.rs` and `src/apply.rs::resource_ref`, not new to this plan.
