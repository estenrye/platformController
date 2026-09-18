# Leader-Election / Multi-Replica HA Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `platform-controller` run multiple replicas safely by adding Lease-based leader election, so a standby can take over reconciliation if the active replica's pod or node fails.

**Architecture:** A background task in every replica runs the Lease acquire/renew/release protocol against `coordination.k8s.io/v1` and writes the result into a shared `Arc<AtomicBool>`. `reconcile()` checks that flag at its very start and no-ops if not leader — every replica runs the identical `Controller` loop unconditionally; only the flag check gates real work. No Controller-loop teardown/rebuild on leadership transitions.

**Tech Stack:** Existing dependencies only — `k8s-openapi`'s `coordination::v1::Lease` type (and its re-exported `jiff::Timestamp` for time arithmetic), `kube`'s typed `Api<Lease>`, `tokio`'s signal handling (`full` feature already enabled). No new Cargo dependencies.

**Spec:** [docs/superpowers/specs/2026-09-18-leader-election-ha-design.md](../specs/2026-09-18-leader-election-ha-design.md)

## Global Constraints

- Lease: name `platform-controller-leader`, namespace `platform-system`.
- Timings: `leaseDurationSeconds: 15`, `renewDeadlineSeconds: 10`, `retryPeriodSeconds: 2`.
- Every replica runs the full, unmodified `Controller` loop at all times. Leadership is enforced only by a check at the very start of `reconcile()`, not by starting/stopping the Controller machinery.
- "Hard cutover, let it finish": no explicit cancellation or drain logic anywhere in this plan. A reconcile that started while leader always runs to completion untouched **when leadership is what changed** — the `is_leader` flag is only read at the start of a reconcile, so nothing interrupts one in progress. This does *not* extend to process shutdown: SIGTERM (Task 4) wins a `tokio::select!` race that drops the `Controller` future, and `kube-runtime` drives reconciles inline on that future rather than on separate tasks, so an in-flight reconcile is cut off wherever it was. That is safe because the successor leader re-applies the full resource set from `status.appliedResources`. See spec §2.
- Deployment: `replicas: 2`, preferred pod anti-affinity on `kubernetes.io/hostname`, a `PodDisruptionBudget` with `maxUnavailable: 1`.
- No RBAC manifest changes — `coordination.k8s.io/leases` is already covered by the existing `cluster-admin` binding — but the permission ledger at `docs/memory/rbac-cluster-admin-tradeoff.md` must gain a row for it.
- SIGTERM triggers a best-effort Lease release (if currently leader) before the process exits.

---

## Task 1: Pure lease-decision logic

**Files:**
- Create: `src/leader.rs`
- Modify: `src/lib.rs` (add `pub mod leader;`)

**Interfaces:**
- Produces: `platform_controller::leader::{LeaseState, LeaseAction, decide_lease_action}` — `decide_lease_action(lease: Option<&LeaseState>, identity: &str, now_unix_seconds: i64, lease_duration_seconds: i64) -> LeaseAction`. Used by Task 2's networked loop.

- [ ] **Step 1: Write the failing tests**

Create `src/leader.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn state(holder: Option<&str>, renew_time: Option<i64>) -> LeaseState {
        LeaseState {
            holder_identity: holder.map(str::to_string),
            renew_time_unix_seconds: renew_time,
            resource_version: "1".to_string(),
        }
    }

    #[test]
    fn no_lease_returns_create() {
        assert_eq!(decide_lease_action(None, "me", 100, 15), LeaseAction::Create);
    }

    #[test]
    fn held_by_self_returns_renew() {
        let lease = state(Some("me"), Some(100));
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 105, 15),
            LeaseAction::Renew {
                resource_version: "1".to_string()
            }
        );
    }

    #[test]
    fn held_by_other_not_expired_returns_wait() {
        let lease = state(Some("other"), Some(100));
        assert_eq!(decide_lease_action(Some(&lease), "me", 110, 15), LeaseAction::Wait);
    }

    #[test]
    fn held_by_other_expired_returns_acquire() {
        let lease = state(Some("other"), Some(100));
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 200, 15),
            LeaseAction::Acquire {
                resource_version: "1".to_string()
            }
        );
    }

    #[test]
    fn held_by_other_with_no_renew_time_returns_acquire() {
        let lease = state(Some("other"), None);
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 100, 15),
            LeaseAction::Acquire {
                resource_version: "1".to_string()
            }
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib leader:: 2>&1 | head -30`
Expected: compile errors — `LeaseState`, `LeaseAction`, `decide_lease_action` do not exist yet.

- [ ] **Step 3: Implement above the test module**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseState {
    pub holder_identity: Option<String>,
    pub renew_time_unix_seconds: Option<i64>,
    pub resource_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAction {
    Create,
    Acquire { resource_version: String },
    Renew { resource_version: String },
    Wait,
}

pub fn decide_lease_action(
    lease: Option<&LeaseState>,
    identity: &str,
    now_unix_seconds: i64,
    lease_duration_seconds: i64,
) -> LeaseAction {
    let Some(state) = lease else {
        return LeaseAction::Create;
    };

    let expired = state
        .renew_time_unix_seconds
        .map(|renew| now_unix_seconds > renew + lease_duration_seconds)
        .unwrap_or(true);

    match &state.holder_identity {
        Some(holder) if holder == identity => LeaseAction::Renew {
            resource_version: state.resource_version.clone(),
        },
        Some(_) if !expired => LeaseAction::Wait,
        _ => LeaseAction::Acquire {
            resource_version: state.resource_version.clone(),
        },
    }
}
```

- [ ] **Step 4: Wire the module into the library**

In `src/lib.rs`:

```rust
pub mod apply;
pub mod crd;
pub mod helm;
pub mod leader;
pub mod manifests;
pub mod reconciler;
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib leader:: 2>&1 | tail -20`
Expected: 5 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/leader.rs
git commit -m "feat: add pure lease-acquisition decision logic"
```

---

## Task 2: Networked lease acquire/renew/release loop

**Files:**
- Modify: `src/leader.rs`

**Interfaces:**
- Consumes: `LeaseState`, `LeaseAction`, `decide_lease_action` (Task 1).
- Produces: `platform_controller::leader::{run, release}` — `pub async fn run(client: kube::Client, namespace: String, lease_name: String, identity: String, is_leader: std::sync::Arc<std::sync::atomic::AtomicBool>)` (loops forever, never returns under normal operation) and `pub async fn release(client: kube::Client, namespace: String, lease_name: String, identity: String)` (best-effort, returns once). Used by Task 4's `main.rs`.

This task's networked functions cannot be meaningfully unit-tested without a live cluster (exercised by Task 7's live-cluster test). The acceptance bar here is `cargo build` succeeding and the crate's existing tests continuing to pass — matching how this codebase has always treated its networked "thin wrapper" functions (e.g. `helm::render`, `apply::apply_object`).

The following k8s-openapi/kube/jiff APIs were verified directly against the resolved crate source in `~/.cargo/registry` before writing this task (matching the project's established practice for exactly this kind of cross-crate risk): `k8s_openapi::api::coordination::v1::{Lease, LeaseSpec}` (fields `holder_identity: Option<String>`, `lease_duration_seconds: Option<i32>`, `renew_time`/`acquire_time: Option<MicroTime>`, `lease_transitions: Option<i32>`), `k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime` (a newtype tuple struct wrapping `k8s_openapi::jiff::Timestamp`, **not** `chrono` — k8s-openapi 0.28.0 re-exports `jiff` publicly via `pub use jiff;`, so it's reachable as `k8s_openapi::jiff::Timestamp` with no new Cargo dependency needed), `jiff::Timestamp::{now, as_second, from_second}` (all present, `Timestamp` is `Copy`), `kube::Api::{get_opt, create, replace, namespaced}`.

- [ ] **Step 1: Implement above the existing test module in `src/leader.rs`**

```rust
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::api::PostParams;
use kube::{Api, Client};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::time::Instant as TokioInstant;

pub const LEASE_DURATION_SECONDS: i64 = 15;
pub const RENEW_DEADLINE: StdDuration = StdDuration::from_secs(10);
pub const RETRY_PERIOD: StdDuration = StdDuration::from_secs(2);

fn lease_state_from(lease: &Lease) -> LeaseState {
    let spec = lease.spec.as_ref();
    LeaseState {
        holder_identity: spec.and_then(|s| s.holder_identity.clone()),
        renew_time_unix_seconds: spec
            .and_then(|s| s.renew_time.as_ref())
            .map(|t| t.0.as_second()),
        resource_version: lease.metadata.resource_version.clone().unwrap_or_default(),
    }
}

fn build_lease(
    name: &str,
    identity: &str,
    lease_duration_seconds: i64,
    now: Timestamp,
    lease_transitions: i32,
    acquire_time: Option<MicroTime>,
    resource_version: Option<String>,
) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            resource_version,
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(identity.to_string()),
            lease_duration_seconds: Some(lease_duration_seconds as i32),
            acquire_time: Some(acquire_time.unwrap_or(MicroTime(now))),
            renew_time: Some(MicroTime(now)),
            lease_transitions: Some(lease_transitions),
            preferred_holder: None,
            strategy: None,
        }),
    }
}

pub async fn run(
    client: Client,
    namespace: String,
    lease_name: String,
    identity: String,
    is_leader: Arc<AtomicBool>,
) {
    let api: Api<Lease> = Api::namespaced(client, &namespace);

    loop {
        let existing = match api.get_opt(&lease_name).await {
            Ok(existing) => existing,
            Err(err) => {
                tracing::warn!(error = %err, "failed to fetch lease, retrying");
                is_leader.store(false, Ordering::Relaxed);
                tokio::time::sleep(RETRY_PERIOD).await;
                continue;
            }
        };

        let now = Timestamp::now();
        let state = existing.as_ref().map(lease_state_from);
        let action = decide_lease_action(state.as_ref(), &identity, now.as_second(), LEASE_DURATION_SECONDS);

        match action {
            LeaseAction::Create => {
                let lease = build_lease(&lease_name, &identity, LEASE_DURATION_SECONDS, now, 0, None, None);
                match api.create(&PostParams::default(), &lease).await {
                    Ok(_) => {
                        tracing::info!(identity = %identity, "acquired leadership (created lease)");
                        is_leader.store(true, Ordering::Relaxed);
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "failed to create lease, likely lost race");
                        is_leader.store(false, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(RETRY_PERIOD).await;
            }
            LeaseAction::Acquire { resource_version } => {
                let transitions = existing
                    .as_ref()
                    .and_then(|lease| lease.spec.as_ref())
                    .and_then(|spec| spec.lease_transitions)
                    .unwrap_or(0)
                    + 1;
                let lease = build_lease(
                    &lease_name,
                    &identity,
                    LEASE_DURATION_SECONDS,
                    now,
                    transitions,
                    None,
                    Some(resource_version),
                );
                match api.replace(&lease_name, &PostParams::default(), &lease).await {
                    Ok(_) => {
                        tracing::info!(identity = %identity, "acquired leadership");
                        is_leader.store(true, Ordering::Relaxed);
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "failed to acquire lease, likely lost race");
                        is_leader.store(false, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(RETRY_PERIOD).await;
            }
            LeaseAction::Renew { resource_version } => {
                hold_and_renew(&api, &lease_name, &identity, existing, resource_version, &is_leader).await;
            }
            LeaseAction::Wait => {
                is_leader.store(false, Ordering::Relaxed);
                tokio::time::sleep(RETRY_PERIOD).await;
            }
        }
    }
}

async fn hold_and_renew(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    mut existing: Option<Lease>,
    mut resource_version: String,
    is_leader: &AtomicBool,
) {
    let deadline = TokioInstant::now() + RENEW_DEADLINE;
    loop {
        let now = Timestamp::now();
        let transitions = existing
            .as_ref()
            .and_then(|lease| lease.spec.as_ref())
            .and_then(|spec| spec.lease_transitions)
            .unwrap_or(0);
        let acquire_time = existing
            .as_ref()
            .and_then(|lease| lease.spec.as_ref())
            .and_then(|spec| spec.acquire_time.clone());
        let lease = build_lease(
            lease_name,
            identity,
            LEASE_DURATION_SECONDS,
            now,
            transitions,
            acquire_time,
            Some(resource_version.clone()),
        );

        match api.replace(lease_name, &PostParams::default(), &lease).await {
            Ok(_) => {
                is_leader.store(true, Ordering::Relaxed);
                tokio::time::sleep(RETRY_PERIOD).await;
                return;
            }
            Err(err) => {
                tracing::warn!(error = %err, "lease renewal failed");
                if TokioInstant::now() >= deadline {
                    tracing::warn!(identity = %identity, "lost leadership after failing to renew within deadline");
                    is_leader.store(false, Ordering::Relaxed);
                    return;
                }
                tokio::time::sleep(RETRY_PERIOD).await;
                match api.get_opt(lease_name).await {
                    Ok(Some(refreshed)) => {
                        resource_version = refreshed.metadata.resource_version.clone().unwrap_or_default();
                        existing = Some(refreshed);
                    }
                    Ok(None) => {
                        is_leader.store(false, Ordering::Relaxed);
                        return;
                    }
                    Err(_) => {
                        // Keep retrying with the same resource_version; it will
                        // conflict-fail again and this loop re-checks the
                        // deadline on the next iteration.
                    }
                }
            }
        }
    }
}

pub async fn release(client: Client, namespace: String, lease_name: String, identity: String) {
    let api: Api<Lease> = Api::namespaced(client, &namespace);
    let existing = match api.get_opt(&lease_name).await {
        Ok(Some(existing)) => existing,
        _ => return,
    };

    let is_holder = existing
        .spec
        .as_ref()
        .and_then(|spec| spec.holder_identity.as_deref())
        == Some(identity.as_str());
    if !is_holder {
        return;
    }

    let expired_time = match Timestamp::from_second(Timestamp::now().as_second() - LEASE_DURATION_SECONDS - 1) {
        Ok(time) => time,
        Err(_) => return,
    };
    let resource_version = existing.metadata.resource_version.clone();
    let lease = Lease {
        metadata: ObjectMeta {
            name: Some(lease_name.clone()),
            resource_version,
            ..Default::default()
        },
        spec: existing.spec.map(|mut spec| {
            spec.renew_time = Some(MicroTime(expired_time));
            spec
        }),
    };

    match api.replace(&lease_name, &PostParams::default(), &lease).await {
        Ok(_) => tracing::info!(identity = %identity, "released lease on shutdown"),
        Err(err) => tracing::warn!(error = %err, "failed to release lease on shutdown"),
    }
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build 2>&1 | tail -30`
Expected: builds with no errors. If any of the API names above don't match what actually compiles, check that symbol's real signature in `~/.cargo/registry/src/*/k8s-openapi-0.28.0/` or `~/.cargo/registry/src/*/kube-client-4.2.0/` and adjust — the shape of the fix is the same either way, only names might differ.

- [ ] **Step 3: Run the existing test suite**

Run: `cargo test --lib leader:: 2>&1 | tail -20`
Expected: the 5 tests from Task 1 still pass unchanged.

- [ ] **Step 4: Commit**

```bash
git add src/leader.rs
git commit -m "feat: implement lease acquire/renew/release against the live cluster"
```

---

## Task 3: Leadership gate in the reconciler

**Files:**
- Modify: `src/reconciler.rs`

**Interfaces:**
- Consumes: nothing new from other tasks.
- Produces: `Context.is_leader: Arc<AtomicBool>` (new field) and `platform_controller::reconciler::leader_gate(is_leader: &AtomicBool) -> Option<kube::runtime::controller::Action>` (private-to-crate is fine; used inline by `reconcile`). Used by Task 4's `main.rs`, which must now construct `Context` with an `is_leader` field.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/reconciler.rs` (find it via `grep -n "mod tests" src/reconciler.rs`):

```rust
    #[test]
    fn leader_gate_returns_requeue_when_not_leader() {
        let is_leader = std::sync::atomic::AtomicBool::new(false);
        assert!(leader_gate(&is_leader).is_some());
    }

    #[test]
    fn leader_gate_returns_none_when_leader() {
        let is_leader = std::sync::atomic::AtomicBool::new(true);
        assert!(leader_gate(&is_leader).is_none());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib reconciler::tests::leader_gate 2>&1 | head -30`
Expected: compile error — `leader_gate` not defined.

- [ ] **Step 3: Add the `is_leader` field and `leader_gate` function**

In `src/reconciler.rs`, change the `Context` struct (currently at line 16-18):

```rust
pub struct Context {
    pub client: Client,
    pub is_leader: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
```

Add this function near `validate` (both are small pure/near-pure helpers):

```rust
fn leader_gate(is_leader: &std::sync::atomic::AtomicBool) -> Option<Action> {
    if is_leader.load(std::sync::atomic::Ordering::Relaxed) {
        None
    } else {
        Some(Action::requeue(Duration::from_secs(15)))
    }
}
```

- [ ] **Step 4: Call the gate at the very start of `reconcile`**

Change the start of `pub async fn reconcile` (currently starting at line 84) from:

```rust
pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
```

to:

```rust
pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    if let Some(action) = leader_gate(&ctx.is_leader) {
        return Ok(action);
    }

    let name = obj.name_any();
```

- [ ] **Step 5: Run to verify it passes, and check for other breakage**

Run: `cargo build 2>&1 | tail -30`
Expected: this will fail to compile at `src/main.rs:21` (`Context { client }` is now missing the `is_leader` field) — that's expected; Task 4 fixes it. Confirm the *only* compile error is that missing field, then run:

Run: `cargo test --lib reconciler::tests::leader_gate 2>&1 | tail -20`
Expected: both new tests pass (this runs even though `main.rs` doesn't compile yet, since `cargo test --lib` only builds the library crate, not the `main.rs` binary).

- [ ] **Step 6: Commit**

```bash
git add src/reconciler.rs
git commit -m "feat: gate reconcile on leadership via a shared flag"
```

---

## Task 4: Wire leader election and graceful shutdown into `main.rs`

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `platform_controller::leader::{run, release}` (Tasks 1-2), `Context { client, is_leader }` (Task 3).

- [ ] **Step 1: Replace `src/main.rs`**

```rust
use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, WatchStreamExt};
use kube::{Api, Client};
use platform_controller::crd::CniInstallation;
use platform_controller::leader;
use platform_controller::reconciler::{error_policy, reconcile, Context};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

const LEASE_NAMESPACE: &str = "platform-system";
const LEASE_NAME: &str = "platform-controller-leader";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = Client::try_default().await?;
    tracing::info!(
        default_namespace = client.default_namespace(),
        "connected to kubernetes"
    );

    let identity = std::env::var("POD_NAME")
        .unwrap_or_else(|_| format!("platform-controller-{}", std::process::id()));

    let is_leader = Arc::new(AtomicBool::new(false));
    tokio::spawn(leader::run(
        client.clone(),
        LEASE_NAMESPACE.to_string(),
        LEASE_NAME.to_string(),
        identity.clone(),
        is_leader.clone(),
    ));

    let api: Api<CniInstallation> = Api::all(client.clone());
    let context = Arc::new(Context {
        client: client.clone(),
        is_leader,
    });

    // The controller's own `update_status` call patches `status` on every
    // successful reconcile, which bumps `resourceVersion` and would otherwise
    // immediately re-trigger reconcile through the watch stream below,
    // starving the intended 300s periodic resync (`Action::requeue` in
    // `reconciler::reconcile`). Kubernetes only increments `metadata.generation`
    // on spec changes, not on status-subresource patches, so filtering the
    // watch stream on generation drops these status-only self-writes while
    // still passing through genuine spec changes.
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

    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = controller => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, releasing lease if held");
            leader::release(client, LEASE_NAMESPACE.to_string(), LEASE_NAME.to_string(), identity).await;
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build 2>&1 | tail -30`
Expected: builds with no errors — this resolves the `Context { client }` breakage left over from Task 3.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test 2>&1 | tail -40`
Expected: all previously-passing tests still pass (22 passed, 2 ignored, before this plan's Task 7 adds a new ignored test).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: run leader election alongside the controller, release lease on SIGTERM"
```

---

## Task 5: Deployment topology — replicas, anti-affinity, PodDisruptionBudget

**Files:**
- Modify: `deploy/bootstrap.yaml`
- Modify: `tests/bootstrap_manifests.rs`

**Interfaces:**
- Consumes: `platform_controller::manifests::{parse_manifests, sort_manifests}` (test only, unchanged from before).

- [ ] **Step 1: Update the Deployment and add a PodDisruptionBudget in `deploy/bootstrap.yaml`**

Replace the existing `Deployment` document (currently lines 35-76) with:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: platform-controller
  namespace: platform-system
spec:
  replicas: 2
  selector:
    matchLabels:
      app: platform-controller
  template:
    metadata:
      labels:
        app: platform-controller
    spec:
      serviceAccountName: platform-controller
      hostNetwork: true
      # Default, not ClusterFirstWithHostNet: this controller runs *before* a CNI
      # exists, so cluster DNS (coredns) cannot be running yet. Pointing at the
      # cluster DNS service IP makes the helm chart-repo lookup fail forever
      # ("lookup projectcalico.docs.tigera.io on 10.96.0.10:53"). Default
      # inherits the node's resolv.conf, which works with no pod network.
      dnsPolicy: Default
      tolerations:
        - key: node.kubernetes.io/not-ready
          operator: Exists
          effect: NoSchedule
        - key: node.kubernetes.io/not-ready
          operator: Exists
          effect: NoExecute
        - key: node-role.kubernetes.io/control-plane
          operator: Exists
          effect: NoSchedule
      nodeSelector:
        node-role.kubernetes.io/control-plane: ""
      # Preferred, not required: a small dev/test cluster may only have one or
      # two control-plane nodes, and a hard requirement could leave a replica
      # permanently unschedulable.
      affinity:
        podAntiAffinity:
          preferredDuringSchedulingIgnoredDuringExecution:
            - weight: 100
              podAffinityTerm:
                topologyKey: kubernetes.io/hostname
                labelSelector:
                  matchLabels:
                    app: platform-controller
      containers:
        - name: platform-controller
          image: platform-controller:latest
          imagePullPolicy: IfNotPresent
          env:
            - name: RUST_LOG
              value: info
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
---
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: platform-controller
  namespace: platform-system
spec:
  maxUnavailable: 1
  selector:
    matchLabels:
      app: platform-controller
```

Leave the final `CniInstallation` document (currently lines 78-95) exactly as-is, after this new `PodDisruptionBudget` document.

- [ ] **Step 2: Update the failing test's expectations**

In `tests/bootstrap_manifests.rs`, change the expected kinds list in `bootstrap_yaml_parses_into_expected_kinds_in_apply_order` from:

```rust
        vec![
            "Namespace",
            "ServiceAccount",
            "ClusterRoleBinding",
            "Deployment",
            "CniInstallation",
        ]
```

to:

```rust
        vec![
            "Namespace",
            "ServiceAccount",
            "ClusterRoleBinding",
            "Deployment",
            "PodDisruptionBudget",
            "CniInstallation",
        ]
```

`PodDisruptionBudget` isn't one of `apply_rank`'s explicitly-ranked kinds (`src/manifests.rs`), so it falls into the same default rank as `CniInstallation`; the stable sort preserves their relative order from the file, which is why `PodDisruptionBudget` is placed immediately after `Deployment` and before `CniInstallation` in the YAML above.

- [ ] **Step 3: Run to verify it passes**

Run: `cargo test --test bootstrap_manifests 2>&1 | tail -20`
Expected: both tests pass.

- [ ] **Step 4: Regenerate the CRD manifest defensively**

Run: `cargo run --bin crdgen > deploy/crd.yaml`
Expected: no changes to `deploy/crd.yaml` (this task doesn't touch the CRD type) — run `git diff deploy/crd.yaml` to confirm it's empty; if it isn't, something unrelated changed and needs investigation before proceeding.

- [ ] **Step 5: Commit**

```bash
git add deploy/bootstrap.yaml tests/bootstrap_manifests.rs
git commit -m "feat: run 2 controller replicas with anti-affinity and a PodDisruptionBudget"
```

---

## Task 6: Update the RBAC permission ledger

**Files:**
- Modify: `docs/memory/rbac-cluster-admin-tradeoff.md`

**Interfaces:** none (documentation only).

- [ ] **Step 1: Add a new row to the permission ledger table**

In `docs/memory/rbac-cluster-admin-tradeoff.md`, in the "Permission ledger" table, add this row immediately after the `platform.rye.ninja` row:

```markdown
| `coordination.k8s.io` | `leases` | Controller itself | Leader-election Lease (`platform-controller-leader` in `platform-system`, added 2026-09-18) — get/create/update only; no list/watch needed since the controller only ever touches its own single named Lease |
```

- [ ] **Step 2: Commit**

```bash
git add docs/memory/rbac-cluster-admin-tradeoff.md
git commit -m "docs: track leader-election Lease permission in the RBAC ledger"
```

---

## Task 7: Live Talos failover test

**Files:**
- Create: `tests/leader_election.rs`

**Interfaces:**
- Consumes: `platform_controller::crd::{CniInstallation, Phase}` (existing), the full leader-election stack wired in Tasks 1-4, and `deploy/bootstrap.yaml`'s 2-replica Deployment (Task 5) applied to a real cluster.

This is the end-to-end proof the failover mechanism actually works, run manually (or as a dedicated, non-default CI job) rather than on every `cargo test` — matching this project's existing `tests/integration_talos.rs`.

- [ ] **Step 1: Write the test**

Create `tests/leader_election.rs`:

```rust
// Run manually against a real Talos-in-Docker cluster with the leader-election
// changes deployed (see deploy/README.md for the image-distribution setup this
// project uses to get a locally-built image into a Talos-in-Docker cluster):
//   talosctl cluster create --name platform-controller-mvp --cni=none --wait
//   export KUBECONFIG=~/.talos/clusters/platform-controller-mvp/kubeconfig
//   kubectl apply -f deploy/crd.yaml
//   kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
//   kubectl apply -f deploy/bootstrap.yaml
//   # wait for both replicas to be Running, then:
//   cargo test --test leader_election -- --ignored --nocapture
//   talosctl cluster destroy --name platform-controller-mvp

use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::crd::{CniInstallation, Phase};
use std::time::Duration;

const NAMESPACE: &str = "platform-system";
const LEASE_NAME: &str = "platform-controller-leader";

async fn current_holder(leases: &Api<Lease>) -> Option<String> {
    leases
        .get_opt(LEASE_NAME)
        .await
        .ok()
        .flatten()
        .and_then(|lease| lease.spec)
        .and_then(|spec| spec.holder_identity)
}

#[tokio::test]
#[ignore = "requires a real Talos cluster with the leader-election bootstrap manifest applied; see module docs for setup"]
async fn killing_the_leader_pod_fails_over_to_a_standby() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the Talos test cluster");

    let leases: Api<Lease> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let installations: Api<CniInstallation> = Api::all(client.clone());

    let acquire_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let original_holder = loop {
        if let Some(holder) = current_holder(&leases).await {
            break holder;
        }
        assert!(
            tokio::time::Instant::now() < acquire_deadline,
            "no replica acquired leadership within 60 seconds"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    pods.delete(&original_holder, &DeleteParams::default())
        .await
        .expect("should be able to delete the leader pod");

    let failover_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(holder) = current_holder(&leases).await {
            if holder != original_holder {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < failover_deadline,
            "no standby took over leadership within 60 seconds of killing {original_holder}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let installation = installations
            .get("default")
            .await
            .expect("default CniInstallation should exist");
        let phase = installation
            .status
            .as_ref()
            .map(|status| status.phase.clone())
            .unwrap_or_default();
        if matches!(phase, Phase::Ready) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "CniInstallation did not return to Ready within 60 seconds of failover"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let pod_list = pods
        .list(&ListParams::default().labels("app=platform-controller"))
        .await
        .expect("should list controller pods");
    assert_eq!(
        pod_list.items.len(),
        2,
        "expected 2 controller pods after failover (Kubernetes should have replaced the deleted one)"
    );
}
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo test --test leader_election --no-run 2>&1 | tail -20`
Expected: compiles cleanly; the test itself is skipped by default (`ignored`).

- [ ] **Step 3: Run it for real against a Talos-in-Docker cluster**

Follow the setup comment at the top of the file (matching `tests/integration_talos.rs`'s established image-distribution approach — a local registry + `--registry-mirror`, since Talos-in-Docker nodes don't automatically see images built directly via `docker build` on the host), then run:
`cargo test --test leader_election -- --ignored --nocapture`
Expected: passes — a standby takes over the Lease within 60 seconds of the original leader's pod being deleted, and the `CniInstallation` returns to `Ready`. Tear down the cluster afterward with `talosctl cluster destroy --name platform-controller-mvp` and remove any registry container you created.

- [ ] **Step 4: Commit**

```bash
git add tests/leader_election.rs
git commit -m "test: add live Talos leader-election failover test"
```

---

## Self-Review Notes

- **Spec coverage:** §1 (Lease protocol, pure/impure split) → Tasks 1-2. §2 (reconcile-time leadership check, no Controller teardown) → Task 3. §3 (replicas, anti-affinity, PDB, RBAC ledger note) → Tasks 5-6. §4 (tracing, SIGTERM release) → Tasks 2 (tracing calls embedded in `run`/`release`) and 4 (SIGTERM wiring). §5 (unit tests + live verification) → Tasks 1 and 7.
- **Placeholder scan:** no TBD/TODO; every step has runnable code and concrete run/verify commands.
- **Type consistency:** `LeaseState`, `LeaseAction`, `decide_lease_action` signatures checked consistent between Task 1 (definition) and Task 2 (consumption). `Context`'s `is_leader` field name and type (`Arc<AtomicBool>`) checked consistent between Task 3 (definition), Task 4 (construction in `main.rs`), and the spec. `leader::run`/`leader::release` signatures checked consistent between Task 2 (definition) and Task 4 (call sites in `main.rs`).
