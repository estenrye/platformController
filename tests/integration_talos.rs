// Run manually against a real Talos-in-Docker cluster:
//   talosctl cluster create --name platform-controller-mvp --cni=none --wait
//   export KUBECONFIG=~/.talos/clusters/platform-controller-mvp/kubeconfig
//   kubectl apply -f deploy/crd.yaml
//   kubectl apply -f deploy/bootstrap.yaml
//   cargo test --test integration_talos -- --ignored --nocapture
//   talosctl cluster destroy --name platform-controller-mvp

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
