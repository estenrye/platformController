# Clean Up Applied Resources on CniInstallation Delete Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deleting a `CniInstallation` should clean up every resource this controller applied, waiting for the CNI provider's own operator to finish tearing down its managed resources before the operator itself is removed.

**Architecture:** Wrap the existing `reconcile` function with `kube::runtime::finalizer`, adding a new `cleanup` function for the `Cleanup` event. `cleanup` deletes `status.appliedResources` in two phases: the provider's own custom resources first (waiting for each to actually disappear, giving its operator time to react), then everything else in reverse apply order. A watch-pipeline gap this surfaces — the current `generation`-only predicate would silently drop the watch event that signals deletion — is fixed alongside it.

**Tech Stack:** `kube::runtime::finalizer` (already available via the existing `kube` dependency's `runtime` feature — no new dependency). `tokio::time::{timeout_at, sleep, Instant}`, already used identically in `src/apply.rs` and `src/leader.rs`.

**Spec:** [docs/superpowers/specs/2026-09-19-cleanup-on-delete-design.md](../specs/2026-09-19-cleanup-on-delete-design.md)

## Global Constraints

- Finalizer name: `platform.rye.ninja/cleanup`.
- New CRD field `spec.cleanupTimeoutSeconds` (`u32`, default `60`) lives at the top level of `CniInstallationSpec` — a sibling of `calico`, not nested under it — since waiting for a provider's own managed resources to disappear isn't Calico-specific.
- Cleanup partitions `status.appliedResources` by rank (reusing `manifests::rank_for_kind`): objects at rank 5 (the catch-all bucket — custom resources like `Installation`/`APIServer`) are deleted first and waited on; everything else (ranks 0-4) is deleted fire-and-forget, in reverse apply order.
- The watch pipeline's predicate becomes `predicates::generation.combine(deletion_requested)`, where `deletion_requested` hashes only whether `metadata.deletionTimestamp` is set — verified against `kube-runtime` 4.2.0's actual source, not assumed.
- `error_policy` requeues at the existing 30s for every variant of `kube::runtime::finalizer::Error<ReconcileError>` — no differentiated handling, matching today's uniform behavior.
- No change to `reconcile`'s existing render/apply/prune/status logic.

---

## Task 1: Add `spec.cleanupTimeoutSeconds` to the CRD

**Files:**
- Modify: `src/crd.rs:14-18` (the `CniInstallationSpec` struct)

**Interfaces:**
- Produces: `CniInstallationSpec.cleanup_timeout_seconds: u32` (serializes as `cleanupTimeoutSeconds`, defaults to `60` when omitted). Used by Task 4's `cleanup` function.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/crd.rs` (alongside the existing `ip_pool_defaults_apply_when_omitted` test):

```rust
    #[test]
    fn cleanup_timeout_seconds_defaults_when_omitted() {
        let json = serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "calico",
            "calico": {
                "chartVersion": "v3.29.1"
            }
        });

        let spec: CniInstallationSpec = serde_json::from_value(json).expect("spec should deserialize");

        assert_eq!(spec.cleanup_timeout_seconds, 60);
    }

    #[test]
    fn cleanup_timeout_seconds_can_be_overridden() {
        let json = serde_json::json!({
            "platformKind": "talos-linux",
            "provider": "calico",
            "calico": {
                "chartVersion": "v3.29.1"
            },
            "cleanupTimeoutSeconds": 5
        });

        let spec: CniInstallationSpec = serde_json::from_value(json).expect("spec should deserialize");

        assert_eq!(spec.cleanup_timeout_seconds, 5);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib crd:: 2>&1 | head -30`
Expected: compile error — no field `cleanup_timeout_seconds` on `CniInstallationSpec`.

- [ ] **Step 3: Add the field**

In `src/crd.rs`, change:

```rust
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CniInstallation",
    status = "CniInstallationStatus",
    shortname = "cni"
)]
#[serde(rename_all = "camelCase")]
pub struct CniInstallationSpec {
    pub platform_kind: PlatformKind,
    pub provider: CniProvider,
    pub calico: CalicoSpec,
}
```

to:

```rust
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "platform.rye.ninja",
    version = "v1alpha1",
    kind = "CniInstallation",
    status = "CniInstallationStatus",
    shortname = "cni"
)]
#[serde(rename_all = "camelCase")]
pub struct CniInstallationSpec {
    pub platform_kind: PlatformKind,
    pub provider: CniProvider,
    pub calico: CalicoSpec,
    /// How long to wait, during cleanup, for the CNI provider's own managed
    /// resources (e.g. Calico's `calico-node` DaemonSet) to actually disappear
    /// before this controller removes the provider's operator itself. Not
    /// nested under `calico`: any future provider's cleanup would need the
    /// same kind of bounded wait.
    #[serde(default = "default_cleanup_timeout_seconds")]
    pub cleanup_timeout_seconds: u32,
}

fn default_cleanup_timeout_seconds() -> u32 {
    60
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib crd:: 2>&1 | tail -20`
Expected: all `crd::` tests pass, including the 2 new ones.

- [ ] **Step 5: Commit**

```bash
git add src/crd.rs
git commit -m "feat: add spec.cleanupTimeoutSeconds to CniInstallation"
```

---

## Task 2: Expose `rank_for_kind` from `manifests.rs`

**Files:**
- Modify: `src/manifests.rs:41-51` (the `apply_rank` function)

**Interfaces:**
- Produces: `platform_controller::manifests::rank_for_kind(kind: &str) -> u8` and `platform_controller::manifests::CUSTOM_RESOURCE_RANK: u8` (value `5`). Used by Task 4's `cleanup`, which only has `AppliedResourceRef` values (a `kind: String` field), not full `DynamicObject`s, so it can't call `apply_rank` directly.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/manifests.rs` (alongside the existing tests):

```rust
    #[test]
    fn rank_for_kind_places_custom_resources_in_the_highest_bucket() {
        assert_eq!(rank_for_kind("Namespace"), 0);
        assert_eq!(rank_for_kind("CustomResourceDefinition"), 1);
        assert_eq!(rank_for_kind("ServiceAccount"), 2);
        assert_eq!(rank_for_kind("ConfigMap"), 3);
        assert_eq!(rank_for_kind("Deployment"), 4);
        assert_eq!(rank_for_kind("Installation"), CUSTOM_RESOURCE_RANK);
        assert_eq!(rank_for_kind("APIServer"), CUSTOM_RESOURCE_RANK);
        assert_eq!(CUSTOM_RESOURCE_RANK, 5);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib manifests:: 2>&1 | head -30`
Expected: compile error — `rank_for_kind`/`CUSTOM_RESOURCE_RANK` not found in this scope.

- [ ] **Step 3: Extract `rank_for_kind`**

In `src/manifests.rs`, change:

```rust
pub fn apply_rank(obj: &DynamicObject) -> u8 {
    let kind = obj.types.as_ref().map(|t| t.kind.as_str()).unwrap_or("");
    match kind {
        "Namespace" => 0,
        "CustomResourceDefinition" => 1,
        "ServiceAccount" | "ClusterRole" | "ClusterRoleBinding" | "Role" | "RoleBinding" => 2,
        "ConfigMap" | "Secret" | "Service" | "ValidatingWebhookConfiguration" | "APIService" => 3,
        "Deployment" | "DaemonSet" => 4,
        _ => 5,
    }
}
```

to:

```rust
/// The rank bucket for any kind not explicitly enumerated below. Chart-managed
/// custom resources (e.g. `Installation`, `APIServer`) always fall here, since
/// they're applied last, after the CRDs and RBAC/workloads that define and run
/// them — and, symmetrically, are the first things cleanup deletes.
pub const CUSTOM_RESOURCE_RANK: u8 = 5;

pub fn rank_for_kind(kind: &str) -> u8 {
    match kind {
        "Namespace" => 0,
        "CustomResourceDefinition" => 1,
        "ServiceAccount" | "ClusterRole" | "ClusterRoleBinding" | "Role" | "RoleBinding" => 2,
        "ConfigMap" | "Secret" | "Service" | "ValidatingWebhookConfiguration" | "APIService" => 3,
        "Deployment" | "DaemonSet" => 4,
        _ => CUSTOM_RESOURCE_RANK,
    }
}

pub fn apply_rank(obj: &DynamicObject) -> u8 {
    let kind = obj.types.as_ref().map(|t| t.kind.as_str()).unwrap_or("");
    rank_for_kind(kind)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib manifests:: 2>&1 | tail -20`
Expected: all `manifests::` tests pass, including the new one and the pre-existing `sorts_namespaces_and_crds_before_workloads_and_operator_crs_last` (behavior-preserving refactor — `apply_rank`'s output is unchanged for every kind).

- [ ] **Step 5: Commit**

```bash
git add src/manifests.rs
git commit -m "refactor: extract rank_for_kind so cleanup can classify AppliedResourceRef values"
```

---

## Task 3: `delete_and_wait_for_removal` in `src/apply.rs`

**Files:**
- Modify: `src/apply.rs:6-24` (the `ApplyError` enum)
- Modify: `src/apply.rs:121-157` (`delete_object`, to factor out its shared discovery logic)

**Interfaces:**
- Produces: `platform_controller::apply::delete_and_wait_for_removal(client: &kube::Client, reference: &AppliedResourceRef, timeout: std::time::Duration) -> Result<(), ApplyError>`, and a new `ApplyError::NotDeleted { kind: String, name: String, timeout: std::time::Duration }` variant. Used by Task 4's `cleanup`.

- [ ] **Step 1: Extend `ApplyError`**

In `src/apply.rs`, add a new variant to the existing `ApplyError` enum:

```rust
    #[error("{kind}/{name} was not removed within {timeout:?}")]
    NotDeleted {
        kind: String,
        name: String,
        timeout: Duration,
    },
```

- [ ] **Step 2: Run to verify the crate still builds**

Run: `cargo build 2>&1 | tail -20`
Expected: builds cleanly (a new, unused-so-far enum variant doesn't break anything).

- [ ] **Step 3: Factor `delete_object`'s discovery + API construction into a shared helper**

In `src/apply.rs`, change `delete_object` from:

```rust
pub async fn delete_object(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<(), ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: reference.api_version.clone(),
        kind: reference.kind.clone(),
    };
    let gvk = group_version_kind(&types);

    let (api_resource, _caps) = kube::discovery::oneshot::pinned_kind(client, &gvk)
        .await
        .map_err(|source| ApplyError::Discovery {
            api_version: reference.api_version.clone(),
            kind: reference.kind.clone(),
            source,
        })?;

    let api: kube::Api<DynamicObject> = if reference.namespace.is_empty() {
        kube::Api::all_with(client.clone(), &api_resource)
    } else {
        kube::Api::namespaced_with(client.clone(), &reference.namespace, &api_resource)
    };

    match api
        .delete(&reference.name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(()),
        Err(source) => Err(ApplyError::Patch {
            kind: reference.kind.clone(),
            name: reference.name.clone(),
            source,
        }),
    }
}
```

to:

```rust
async fn dynamic_api_for(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<kube::Api<DynamicObject>, ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: reference.api_version.clone(),
        kind: reference.kind.clone(),
    };
    let gvk = group_version_kind(&types);

    let (api_resource, _caps) = kube::discovery::oneshot::pinned_kind(client, &gvk)
        .await
        .map_err(|source| ApplyError::Discovery {
            api_version: reference.api_version.clone(),
            kind: reference.kind.clone(),
            source,
        })?;

    Ok(if reference.namespace.is_empty() {
        kube::Api::all_with(client.clone(), &api_resource)
    } else {
        kube::Api::namespaced_with(client.clone(), &reference.namespace, &api_resource)
    })
}

pub async fn delete_object(
    client: &kube::Client,
    reference: &AppliedResourceRef,
) -> Result<(), ApplyError> {
    let api = dynamic_api_for(client, reference).await?;

    match api
        .delete(&reference.name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(()),
        Err(source) => Err(ApplyError::Patch {
            kind: reference.kind.clone(),
            name: reference.name.clone(),
            source,
        }),
    }
}
```

- [ ] **Step 4: Run to verify the refactor is behavior-preserving**

Run: `cargo test --lib apply:: 2>&1 | tail -20`
Expected: all existing `apply::` tests still pass (this step is a pure refactor — no test should change behavior).

- [ ] **Step 5: Add `delete_and_wait_for_removal`**

In `src/apply.rs`, add this function after `delete_object`:

```rust
pub async fn delete_and_wait_for_removal(
    client: &kube::Client,
    reference: &AppliedResourceRef,
    timeout: Duration,
) -> Result<(), ApplyError> {
    delete_object(client, reference).await?;

    let api = dynamic_api_for(client, reference).await?;
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        match tokio::time::timeout_at(deadline, api.get_opt(&reference.name)).await {
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(_))) => {}
            Ok(Err(source)) => {
                tracing::debug!(
                    kind = %reference.kind,
                    name = %reference.name,
                    error = %source,
                    "failed to check whether resource was removed"
                );
            }
            Err(_) => {
                return Err(ApplyError::NotDeleted {
                    kind: reference.kind.clone(),
                    name: reference.name.clone(),
                    timeout,
                });
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ApplyError::NotDeleted {
                kind: reference.kind.clone(),
                name: reference.name.clone(),
                timeout,
            });
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
```

This mirrors `wait_for_crd_established`'s already-reviewed shape (the underlying API call — `get_opt` here — is itself bounded by `tokio::time::timeout_at`, not just the surrounding loop's deadline check, which was a real defect a final review caught and fixed in that earlier function). `delete_and_wait_for_removal`'s own polling loop is networked and not unit-tested, matching that same precedent — it's covered by this plan's Task 5 live verification instead.

- [ ] **Step 6: Run to verify the crate builds and existing tests pass**

Run: `cargo build 2>&1 | tail -20 && cargo test --lib apply:: 2>&1 | tail -20`
Expected: clean build, all `apply::` tests pass. No new unit test is added in this step for `delete_and_wait_for_removal` itself — same established pattern as `wait_for_crd_established`'s own networked loop.

- [ ] **Step 7: Commit**

```bash
git add src/apply.rs
git commit -m "feat: add delete_and_wait_for_removal for provider-managed cleanup ordering"
```

---

## Task 4: `cleanup` and resource partitioning in `src/reconciler.rs`

**Files:**
- Modify: `src/reconciler.rs` (add new functions; no changes to the existing `reconcile`, `validate`, `leader_gate`, or `update_status`)

**Interfaces:**
- Consumes: `crate::manifests::rank_for_kind`, `crate::manifests::CUSTOM_RESOURCE_RANK` (Task 2); `crate::apply::delete_and_wait_for_removal`, `crate::apply::delete_object` (Task 3, the latter already existed); `CniInstallationSpec.cleanup_timeout_seconds` (Task 1).
- Produces: `platform_controller::reconciler::cleanup(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError>`. Used by Task 5's `reconcile_with_finalizer`.

- [ ] **Step 1: Write the failing test for partitioning**

Add to the `tests` module in `src/reconciler.rs` (alongside the existing tests):

```rust
    fn applied_resource(kind: &str, name: &str) -> AppliedResourceRef {
        AppliedResourceRef {
            api_version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn partition_for_cleanup_separates_custom_resources_from_infra() {
        let namespace = applied_resource("Namespace", "tigera-operator");
        let crd = applied_resource("CustomResourceDefinition", "installations.operator.tigera.io");
        let deployment = applied_resource("Deployment", "tigera-operator");
        let installation = applied_resource("Installation", "default");

        let (custom_resources, infra) = partition_for_cleanup(&[
            namespace.clone(),
            crd.clone(),
            deployment.clone(),
            installation.clone(),
        ]);

        assert_eq!(custom_resources, vec![installation]);
        assert_eq!(infra, vec![namespace, crd, deployment]);
    }

    #[test]
    fn partition_for_cleanup_handles_no_custom_resources() {
        let namespace = applied_resource("Namespace", "tigera-operator");

        let (custom_resources, infra) = partition_for_cleanup(&[namespace.clone()]);

        assert!(custom_resources.is_empty());
        assert_eq!(infra, vec![namespace]);
    }

    #[test]
    fn partition_for_cleanup_handles_empty_input() {
        let (custom_resources, infra) = partition_for_cleanup(&[]);

        assert!(custom_resources.is_empty());
        assert!(infra.is_empty());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib reconciler:: 2>&1 | head -30`
Expected: compile error — `partition_for_cleanup` not found in this scope.

- [ ] **Step 3: Implement `partition_for_cleanup` and `cleanup`**

In `src/reconciler.rs`, add these after `update_status` (before the `#[cfg(test)]` module):

```rust
fn partition_for_cleanup(
    resources: &[AppliedResourceRef],
) -> (Vec<AppliedResourceRef>, Vec<AppliedResourceRef>) {
    resources
        .iter()
        .cloned()
        .partition(|resource| crate::manifests::rank_for_kind(&resource.kind) == crate::manifests::CUSTOM_RESOURCE_RANK)
}

pub async fn cleanup(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
    let applied = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if applied.is_empty() {
        tracing::info!(installation = %name, "nothing to clean up");
        return Ok(Action::await_change());
    }

    let (custom_resources, infra) = partition_for_cleanup(&applied);
    let timeout = Duration::from_secs(u64::from(obj.spec.cleanup_timeout_seconds));

    for reference in custom_resources.iter().rev() {
        tracing::info!(
            installation = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting provider-managed resource and waiting for removal"
        );
        crate::apply::delete_and_wait_for_removal(&ctx.client, reference, timeout).await?;
    }

    for reference in infra.iter().rev() {
        tracing::info!(
            installation = %name,
            kind = %reference.kind,
            resource = %reference.name,
            "deleting applied resource"
        );
        crate::apply::delete_object(&ctx.client, reference).await?;
    }

    tracing::info!(installation = %name, "cleanup complete");
    Ok(Action::await_change())
}
```

`leader_gate`'s own short-circuit behavior is already covered by the existing `leader_gate_returns_requeue_when_not_leader`/`leader_gate_returns_none_when_leader` tests — `cleanup` calls the exact same function, so no separate test is added here for that branch, matching how `reconcile` itself doesn't re-test it either.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib reconciler:: 2>&1 | tail -30`
Expected: all `reconciler::` tests pass, including the 3 new ones.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: no regressions. `cleanup` is not yet called from anywhere (that's Task 5), so this is inert but fully tested new code — an unused `pub` function does not trigger a dead-code warning in Rust.

- [ ] **Step 6: Commit**

```bash
git add src/reconciler.rs
git commit -m "feat: add cleanup with rank-based deletion ordering"
```

---

## Task 5: Wire the finalizer into the Controller and fix the watch-pipeline delete gap

**Files:**
- Modify: `src/reconciler.rs` (add `reconcile_with_finalizer`; change `error_policy`'s signature)
- Modify: `src/main.rs` (add `deletion_requested`; update the watch pipeline and the `Controller::run` call)

**Interfaces:**
- Consumes: `crate::reconciler::{reconcile, cleanup}` (existing + Task 4).
- Produces: `platform_controller::reconciler::reconcile_with_finalizer(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, kube::runtime::finalizer::Error<ReconcileError>>`, `platform_controller::reconciler::FINALIZER_NAME: &str`. `error_policy`'s signature changes to accept `&kube::runtime::finalizer::Error<ReconcileError>` instead of `&ReconcileError` — this is why this task changes both files together: `main.rs`'s `Controller::run(reconcile, error_policy, context)` call needs `reconcile_with_finalizer` and the new `error_policy` signature swapped in during the same commit, or the crate won't build in between.

This task's two files are edited together because `error_policy`'s signature change and `main.rs`'s `.run()` call are two halves of one atomic change — the crate does not compile with only one side updated.

- [ ] **Step 1: Write the failing test for the new watch predicate**

Add a `#[cfg(test)] mod tests` block to `src/main.rs` (it currently has none — `cargo test` already runs a `platform_controller` binary test target, per the existing `running 0 tests` output for `src/main.rs`, so this is recognized automatically):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_requested_is_none_without_a_deletion_timestamp() {
        let installation: CniInstallation = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CniInstallation",
            "metadata": { "name": "default" },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "calico",
                "calico": { "chartVersion": "v3.29.1" }
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), None);
    }

    #[test]
    fn deletion_requested_is_some_once_deletion_timestamp_is_set() {
        let installation: CniInstallation = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CniInstallation",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-19T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "calico",
                "calico": { "chartVersion": "v3.29.1" }
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), Some(1));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin platform-controller 2>&1 | head -30`
Expected: compile error — `deletion_requested` not found in this scope. (If the binary target name differs, run `cargo test 2>&1 | grep "Running unittests src/main.rs"` first to confirm the invocation; adjust the `--bin` flag to match.)

- [ ] **Step 3: Add `reconcile_with_finalizer` and update `error_policy` in `src/reconciler.rs`**

Add `FINALIZER_NAME` and `reconcile_with_finalizer` after `cleanup`:

```rust
pub const FINALIZER_NAME: &str = "platform.rye.ninja/cleanup";

pub async fn reconcile_with_finalizer(
    obj: Arc<CniInstallation>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<ReconcileError>> {
    let api: kube::Api<CniInstallation> = kube::Api::all(ctx.client.clone());
    kube::runtime::finalizer(&api, FINALIZER_NAME, obj, |event| async move {
        match event {
            kube::runtime::finalizer::Event::Apply(obj) => reconcile(obj, ctx).await,
            kube::runtime::finalizer::Event::Cleanup(obj) => cleanup(obj, ctx).await,
        }
    })
    .await
}
```

Change the existing `error_policy` from:

```rust
pub fn error_policy(_obj: Arc<CniInstallation>, _err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    Action::requeue(Duration::from_secs(30))
}
```

to:

```rust
pub fn error_policy(
    _obj: Arc<CniInstallation>,
    _err: &kube::runtime::finalizer::Error<ReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}
```

- [ ] **Step 4: Update `src/main.rs`**

Change the imports from:

```rust
use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, WatchStreamExt};
use kube::{Api, Client};
use platform_controller::crd::CniInstallation;
use platform_controller::leader;
use platform_controller::reconciler::{error_policy, reconcile, Context};
```

to:

```rust
use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, Predicate, WatchStreamExt};
use kube::{Api, Client, Resource};
use platform_controller::crd::CniInstallation;
use platform_controller::leader;
use platform_controller::reconciler::{error_policy, reconcile_with_finalizer, Context};
```

Add this function before `fn main()`:

```rust
/// `predicates::generation` alone misses deletions: `kubectl delete` on an
/// object with a finalizer only sets `metadata.deletionTimestamp`, which,
/// like a status-subresource write, does not bump `metadata.generation`
/// (verified against `kube-runtime` 4.2.0's own source). Combined below with
/// `predicates::generation` so both spec changes and deletion requests pass
/// through the filter, while status-only self-writes still don't.
fn deletion_requested(obj: &CniInstallation) -> Option<u64> {
    obj.meta().deletion_timestamp.is_some().then_some(1)
}
```

Change the watch pipeline and `Controller::run` call from:

```rust
    let (reader, writer) = reflector::store();
    let installations = watcher(api, watcher::Config::default())
        .default_backoff()
        .reflect(writer)
        .applied_objects()
        .predicate_filter(predicates::generation, Default::default());

    let controller = Controller::for_stream(installations, reader)
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        });
```

to:

```rust
    let (reader, writer) = reflector::store();
    let installations = watcher(api, watcher::Config::default())
        .default_backoff()
        .reflect(writer)
        .applied_objects()
        .predicate_filter(predicates::generation.combine(deletion_requested), Default::default());

    let controller = Controller::for_stream(installations, reader)
        .run(reconcile_with_finalizer, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        });
```

- [ ] **Step 5: Run to verify it builds and the new tests pass**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build.

Run: `cargo test 2>&1 | tail -40`
Expected: all tests pass, including the 2 new `deletion_requested` tests, with no regressions anywhere else.

- [ ] **Step 6: Commit**

```bash
git add src/reconciler.rs src/main.rs
git commit -m "feat: wrap reconcile with a finalizer and fix the watch-pipeline delete gap"
```

- [ ] **Step 7: Live-verify against the real cluster**

This is the step that actually proves the fix — the unit tests only confirm the predicate and partitioning logic, not that deletion is detected promptly and resources are actually removed in the right order on real infrastructure.

1. On the live Talos cluster (`export KUBECONFIG=~/.kube/kubeconfig`), roll the controller Deployment onto a build that includes this fix (see `deploy/README.md`/`tests/integration_talos.rs` for the local-registry-mirror approach if the cluster can't reach Docker Hub, or a freshly tagged image otherwise — note `:latest` with `imagePullPolicy: IfNotPresent` won't be re-pulled by a node that already cached the old image).
2. Apply `examples/cni-installation.yaml` if not already present, and wait for `status.phase` to reach `Ready`.
3. Run `kubectl delete cniinstallation default` and immediately run `kubectl get cniinstallation default -o jsonpath='{.metadata.deletionTimestamp}'` — confirm a timestamp appears right away, not after a long delay (proves the watch-pipeline fix; before this fix, this could take up to 5 minutes since the periodic resync was the only other trigger).
4. Watch the controller's logs (`kubectl logs -n platform-system -l app=platform-controller -f`) and confirm: the `Installation` custom resource is deleted and waited on first, `calico-node` pods actually terminate (`kubectl get pods -n calico-system -w`) before the `tigera-operator` Deployment is removed, then the rest (RBAC, CRDs, namespace) is deleted.
5. Confirm the `CniInstallation` object itself disappears (`kubectl get cniinstallation default` returns not-found) once cleanup finishes, and that every resource this controller applied is gone: `kubectl get namespace tigera-operator` (not found), `kubectl get crd | grep tigera` (not found).
6. Re-apply `examples/cni-installation.yaml` and confirm a fresh install reaches `Ready` cleanly — this round-trip check confirms deletion didn't leave stuck state (e.g. a lingering finalizer on some other object) behind.
7. Record the actual outcome — do not claim success without having watched this happen.

---

## Self-Review Notes

- **Spec coverage:** the spec's four Design subsections map directly: §1 (finalizer wrapper) and §2 (watch-pipeline fix) → Task 5; §3 (CRD field) → Task 1; §4 (cleanup sequencing) → Tasks 2-4. The Testing section's unit-test items map to each task's own Step 1/4; the live-verification item maps to Task 5 Step 7.
- **Placeholder scan:** no TBD/TODO; the live-verification step gives concrete commands and an explicit "record the actual outcome" instruction, matching the pattern from the two prior plans in this series.
- **Type consistency:** `cleanup_timeout_seconds: u32` (Task 1) is read via `u64::from(obj.spec.cleanup_timeout_seconds)` in Task 4's `cleanup` — verified this is a valid widening conversion, no truncation. `rank_for_kind`/`CUSTOM_RESOURCE_RANK` (Task 2) are used identically in Task 4's `partition_for_cleanup`. `delete_and_wait_for_removal`'s signature (Task 3) matches its call site in Task 4 exactly (`&ctx.client, reference, timeout`). `cleanup`'s signature (Task 4) matches its use in Task 5's `reconcile_with_finalizer` exactly (`Arc<CniInstallation>, Arc<Context>) -> Result<Action, ReconcileError>`, which is what `Event::Cleanup(obj) => cleanup(obj, ctx).await` expects to unify with `reconcile`'s identical return type inside the same `async move` block).
- **Verified against source, not assumed:** `kube::runtime::finalizer`'s exact signature and `Event`/`Error` shapes, `Action::await_change()`, `Api::get_opt`, `kube::Resource`/`kube::runtime::Predicate`'s re-export paths, and `predicates::generation`'s exact hashed field were all checked directly against the pinned `kube` 4.2.0 source during design, not assumed from memory or general kube-rs familiarity.
