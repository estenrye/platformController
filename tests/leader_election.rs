// Run manually against a real Talos-in-Docker cluster with the leader-election
// changes deployed. Verified against talosctl v1.4.6 (Kubernetes v1.27.3) on
// Docker; this follows the same proven recipe as tests/integration_talos.rs
// (see that file's header for the full rationale), just with the 2-replica
// bootstrap manifest already committed in deploy/bootstrap.yaml:
//
// talosctl has no `--cni` flag; CNI is disabled with a machine-config patch, and
// `--wait` must be off because the cluster cannot become "ready" until this
// controller installs a CNI:
//
//   cat > /tmp/cni-none.yaml <<'EOF'
//   cluster:
//     network:
//       cni:
//         name: none
//   EOF
//
// The controller image is not on a registry the cluster can reach, so serve it
// from a local registry and point the nodes at it with a registry mirror:
//
//   docker run -d --name platform-registry -p 5005:5000 registry:2
//   docker build -t platform-controller:latest .
//   docker tag platform-controller:latest localhost:5005/platform-controller:latest
//   docker push localhost:5005/platform-controller:latest
//
//   talosctl cluster create --name platform-controller-mvp --workers 1 \
//     --wait=false --config-patch @/tmp/cni-none.yaml \
//     --registry-mirror registry.local:5005=http://10.5.0.1:5005
//
// 10.5.0.1 is the gateway of the cluster network talosctl creates, i.e. the host
// as seen from the nodes. Then fetch a kubeconfig; the node IP is not routable
// from the host, so retarget it at the published API port:
//
//   talosctl --nodes 10.5.0.2 --endpoints 127.0.0.1 kubeconfig /tmp/kubeconfig --force
//   export KUBECONFIG=/tmp/kubeconfig
//   kubectl config set-cluster platform-controller-mvp --server=https://127.0.0.1:6443
//
//   kubectl apply -f deploy/crd.yaml
//   kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
//   sed 's|image: estenrye/platform-controller:latest|image: registry.local:5005/platform-controller:latest|' \
//     deploy/bootstrap.yaml | kubectl apply -f -
//   kubectl apply -f examples/cni-installation.yaml
//   # wait for both replicas to be Running, then:
//   cargo test --test leader_election -- --ignored --nocapture --test-threads=1
//
//   talosctl cluster destroy --name platform-controller-mvp
//   docker rm -f platform-registry
//
// `--test-threads=1` is REQUIRED, not cosmetic: all three tests in this file
// mutate the same singleton Lease and the same two pods, so running them
// concurrently (cargo's default) makes them fight each other and fail
// nondeterministically. Verified: serially, all three pass in ~20s total.
//
// Note on iterating on the controller itself: the Deployment uses
// `imagePullPolicy: IfNotPresent` with the `:latest` tag, so re-pushing
// `:latest` and deleting the pods does NOT pick up a rebuilt image — the nodes
// keep the cached layer. Push a fresh tag and roll onto it instead:
//
//   docker tag platform-controller:latest localhost:5005/platform-controller:t2
//   docker push localhost:5005/platform-controller:t2
//   kubectl set image -n platform-system deployment/platform-controller \
//     platform-controller=registry.local:5005/platform-controller:t2
//   kubectl rollout status -n platform-system deployment/platform-controller
//   # confirm the digest actually changed:
//   kubectl get pods -n platform-system -l app=platform-controller \
//     -o custom-columns='NAME:.metadata.name,IMAGEID:.status.containerStatuses[0].imageID'

use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
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

/// Poll until exactly two live (non-`Terminating`) controller pods exist.
///
/// A single point-in-time `List` right after a delete is flaky: the deleted pod
/// keeps showing up until its grace period elapses and its replacement may not
/// have been created yet, so the raw count transiently reads 1 or 3. Filtering
/// on an absent `metadata.deletionTimestamp` and polling to a deadline asserts
/// the steady state the test actually cares about.
async fn wait_for_two_live_controller_pods(pods: &Api<Pod>, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let live = pods
            .list(&ListParams::default().labels("app=platform-controller"))
            .await
            .expect("should list controller pods")
            .items
            .into_iter()
            .filter(|pod| pod.metadata.deletion_timestamp.is_none())
            .count();
        if live == 2 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected 2 live controller pods within {}s after failover (Kubernetes should have \
             replaced the deleted one), last saw {live}",
            within.as_secs()
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The *graceful* failover path: an ordinary pod delete sends SIGTERM, the
/// leader's handler runs `leader::release`, and a standby picks the lease up in
/// roughly one `RETRY_PERIOD` (~2s). See
/// `a_lease_that_stops_being_renewed_expires_and_a_standby_takes_over` for the
/// genuine expiry-based counterpart (this test's own "ungraceful" sibling,
/// `force_deleting_the_leader_pod_with_zero_grace_still_fails_over`, turns out
/// to still be graceful in practice — see its doc comment).
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
        if current_holder(&leases)
            .await
            .is_some_and(|holder| holder != original_holder)
        {
            break;
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

    wait_for_two_live_controller_pods(&pods, Duration::from_secs(60)).await;
}

/// The lease-expiry path: the spec's primary goal, and the one no other test in
/// this file reaches.
///
/// Both other tests hand over via `leader::release`, which *backdates* `renewTime`
/// to an already-expired value. Takeover is therefore immediate and the
/// `now > renewTime + leaseDurationSeconds` arithmetic in `decide_lease_action`
/// is never actually required to elapse. This test forces that arithmetic to do
/// the work: it writes the Lease over with a holder identity belonging to no pod
/// and a `renewTime` of *now*, which is exactly the state the apiserver is left
/// in by a leader that acquired and then died or was partitioned without
/// releasing. Nothing will ever renew it, so a live replica can only take over by
/// waiting out the full `leaseDurationSeconds` and then winning the `Acquire`
/// race. Bound: 15s lease + 10s renew deadline + polling slack, hence 90s.
///
/// Why not actually kill the process ungracefully? Three mechanisms were tried
/// against a live cluster and none of them work from a test:
///   * `DeleteParams::grace_period(0)` — SIGTERM still lands first and
///     `release()` wins (see the test below).
///   * `kubectl exec ... kill -STOP 1` / `kill -9 1` — the kernel discards
///     SIGSTOP and SIGKILL sent to a PID namespace's init process from *inside*
///     that namespace, so the container's PID 1 is immune. Verified: after
///     SIGSTOP, `/proc/1/status` still reported `State: S (sleeping)` and the
///     lease kept being renewed for 84s.
///   * signalling from the node — Talos nodes ship no shell (`docker exec ... sh`
///     fails with "executable file not found"), which is the point of Talos.
/// Rewriting the Lease is the one mechanism that reproduces the *observable
/// state* a crashed leader leaves behind, and it needs no cluster-internal
/// access, so it works against any cluster rather than only Talos-in-Docker.
#[tokio::test]
#[ignore = "requires a real Talos cluster with the leader-election bootstrap manifest applied; see module docs for setup"]
async fn a_lease_that_stops_being_renewed_expires_and_a_standby_takes_over() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the Talos test cluster");

    let leases: Api<Lease> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let installations: Api<CniInstallation> = Api::all(client.clone());

    let acquire_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if current_holder(&leases).await.is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < acquire_deadline,
            "no replica acquired leadership within 60 seconds"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Stand in for a leader that died without releasing: a holder that exists
    // only in the Lease, with a renewTime that is current as of right now.
    // No `resourceVersion` is set on the write below: this makes it an
    // unconditional PUT (Kubernetes skips the optimistic-concurrency check
    // when the incoming object's resourceVersion is empty), so a real
    // replica's renewal landing in the fetch-then-write gap can never turn
    // this into a spurious 409 — whichever write lands last simply wins,
    // which is fine here since we only care about the end state.
    const PHANTOM_HOLDER: &str = "platform-controller-phantom-crashed-leader";
    let existing = leases
        .get(LEASE_NAME)
        .await
        .expect("leader lease should exist");
    let mut spec = existing.spec.clone().unwrap_or_default();
    spec.holder_identity = Some(PHANTOM_HOLDER.to_string());
    spec.renew_time = Some(MicroTime(Timestamp::now()));
    spec.lease_duration_seconds = Some(15);
    let overwritten = Lease {
        metadata: ObjectMeta {
            name: Some(LEASE_NAME.to_string()),
            ..Default::default()
        },
        spec: Some(spec),
    };
    leases
        .replace(LEASE_NAME, &PostParams::default(), &overwritten)
        .await
        .expect("should be able to hand the lease to a phantom holder");
    let phantom_write_completed = tokio::time::Instant::now();

    // The phantom never renews, so this can only resolve once the lease has
    // genuinely aged out and a real replica has won an Acquire.
    let failover_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let new_holder = loop {
        if let Some(holder) = current_holder(&leases)
            .await
            .filter(|holder| holder.as_str() != PHANTOM_HOLDER)
        {
            break holder;
        }
        assert!(
            tokio::time::Instant::now() < failover_deadline,
            "no replica took the lease over from the phantom holder within 90 seconds"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    // This is the assertion that actually proves genuine expiry-arithmetic ran,
    // rather than some instant-takeover path (e.g. a regression of the
    // stand-down-on-holder-mismatch check in `hold_and_renew`, which is what
    // makes this test meaningful at all — see its own comment in src/leader.rs).
    // 10s rather than the full 15s leaves slack for clock skew between the test
    // host (which stamps `renewTime`) and the cluster nodes (which evaluate the
    // expiry arithmetic).
    let elapsed_since_phantom_write = phantom_write_completed.elapsed();
    assert!(
        elapsed_since_phantom_write >= Duration::from_secs(10),
        "failover completed in {elapsed_since_phantom_write:?}, too fast to have gone through \
         genuine lease expiry (expected >= 10s against a 15s lease duration) — this test is \
         supposed to prove the expiry path ran, not just that failover happened"
    );

    let live_pods = pods
        .list(&ListParams::default().labels("app=platform-controller"))
        .await
        .expect("should list controller pods");
    assert!(
        live_pods
            .items
            .iter()
            .any(|pod| pod.metadata.name.as_deref() == Some(new_holder.as_str())),
        "the new holder {new_holder} should be one of the live controller pods, \
         not another phantom"
    );

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

    wait_for_two_live_controller_pods(&pods, Duration::from_secs(60)).await;
}

/// A zero-grace-period delete of the leader also hands over cleanly.
///
/// This was originally written expecting `grace_period(0)` to deny the leader
/// any chance to run its SIGTERM handler, thereby forcing the lease-expiry path.
/// Measured against a real cluster, it does not: the kubelet still emits SIGTERM
/// before SIGKILL, and because this controller is `hostNetwork` and the apiserver
/// is on the same node, `leader::release`'s get+replace round trip reliably wins
/// that race. Observed handover was ~1.2s with `leaseTransitions` advancing by
/// exactly 1, i.e. the *graceful* path again, not expiry.
///
/// The test is kept because `--grace-period=0` is a thing operators do and it
/// should not regress, but the genuine expiry path is covered by
/// `a_lease_that_stops_being_renewed_expires_and_a_standby_takes_over` above.
/// The 90s deadline is retained so this test still passes if a future change
/// does push it onto the slower expiry path.
#[tokio::test]
#[ignore = "requires a real Talos cluster with the leader-election bootstrap manifest applied; see module docs for setup"]
async fn force_deleting_the_leader_pod_with_zero_grace_still_fails_over() {
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

    pods.delete(&original_holder, &DeleteParams::default().grace_period(0))
        .await
        .expect("should be able to force-delete the leader pod");

    let failover_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if current_holder(&leases)
            .await
            .is_some_and(|holder| holder != original_holder)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < failover_deadline,
            "no standby took over leadership within 90 seconds of force-killing {original_holder}"
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

    wait_for_two_live_controller_pods(&pods, Duration::from_secs(60)).await;
}
