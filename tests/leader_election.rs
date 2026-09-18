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
//   sed 's|image: platform-controller:latest|image: registry.local:5005/platform-controller:latest|' \
//     deploy/bootstrap.yaml | kubectl apply -f -
//   # wait for both replicas to be Running, then:
//   cargo test --test leader_election -- --ignored --nocapture
//
//   talosctl cluster destroy --name platform-controller-mvp
//   docker rm -f platform-registry

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
/// `force_killing_the_leader_pod_fails_over_via_lease_expiry` for the ungraceful
/// counterpart.
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

/// The *ungraceful* failover path, and the spec's actual primary goal: a leader
/// that crashes or is network-partitioned never gets to run its SIGTERM handler,
/// so `leader::release` never happens and takeover has to fall out of the lease
/// simply going stale. The graceful test above cannot prove this — its ~2s
/// handover is entirely due to `release()`, which masks whether the expiry path
/// works at all.
///
/// A zero-grace-period delete is the closest reproduction available from the
/// API: the kubelet SIGKILLs the container immediately instead of allowing a
/// shutdown window, so no `release()` write reaches the apiserver. Takeover is
/// then bounded by `LEASE_DURATION_SECONDS` (15s) plus the standby's own
/// `RENEW_DEADLINE`/`RETRY_PERIOD` polling granularity, not by ~2s — hence the
/// deliberately roomier 90s failover deadline here.
#[tokio::test]
#[ignore = "requires a real Talos cluster with the leader-election bootstrap manifest applied; see module docs for setup"]
async fn force_killing_the_leader_pod_fails_over_via_lease_expiry() {
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
