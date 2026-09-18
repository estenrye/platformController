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
