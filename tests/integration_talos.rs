// Run manually against a real Talos-in-Docker cluster. Verified against
// talosctl v1.4.6 (Kubernetes v1.27.3) on Docker.
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
//
//   cargo test --test integration_talos -- --ignored --nocapture
//
//   talosctl cluster destroy --name platform-controller-mvp
//   docker rm -f platform-registry

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::Client;
use platform_controller::crd::{CniInstallation, Phase};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires a real Talos cluster with no CNI; see module docs for setup"]
async fn calico_becomes_ready_and_nodes_go_ready() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the Talos test cluster");

    let installations: Api<CniInstallation> = Api::all(client.clone());
    let nodes: Api<Node> = Api::all(client.clone());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
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
            tokio::time::Instant::now() < deadline,
            "CniInstallation did not reach Ready within 5 minutes"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let node_list = nodes.list(&ListParams::default()).await.expect("should list nodes");
    for node in node_list.items {
        let ready = node
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .into_iter()
            .flatten()
            .any(|condition| condition.type_ == "Ready" && condition.status == "True");
        assert!(ready, "node {:?} did not become Ready", node.metadata.name);
    }
}
