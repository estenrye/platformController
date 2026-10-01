// Run manually against a real cluster whose apiserver already uses a KMS
// provider (see docs/runbooks/etcd-encryption-verification.md). With the
// controller running and all seven CRDs Established:
//
//   kubectl apply -f examples/etcd-encryption.yaml
//   cargo test --test integration_etcd_encryption -- --ignored --nocapture
//
// This asserts only that, within 180s, the EtcdEncryption "default" reports at
// least one status.nodes[] entry, that every entry has a non-empty name and
// address, and that status.rewrite.total is 0 (the example leaves
// rewrite: Disabled, so this test never rewrites). It prints each node's
// verified flag, writer prefix and reason. It does not assert any particular
// phase. It changes nothing in the cluster apart from the controller's canary
// Secret in kube-system.
//
// Getting a node to `verified` (and a meaningful status at all) requires the
// controller pod to be able to reach each control-plane node's apiserver at
// https://<InternalIP>:6443, and the apiserver's serving certificate must
// accept the server name `kubernetes.default.svc` (unverified). Without that
// the node is still listed, with verified=false and a "cannot verify" reason,
// which this test prints; it only fails if no per-node entry appears at all
// (for example the controller is not running or is not the leader).

use kube::api::Api;
use kube::Client;
use platform_controller::etcd_encryption::EtcdEncryption;
use std::time::Duration;

#[tokio::test]
#[ignore = "needs a live cluster with the controller running; see the header comment"]
async fn observes_every_control_plane_apiserver() {
    let client = Client::try_default().await.expect("a kubeconfig for the test cluster");
    let encryptions: Api<EtcdEncryption> = Api::all(client.clone());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let status = loop {
        let current = encryptions.get("default").await.expect("apply examples/etcd-encryption.yaml first");
        if let Some(status) = current.status.filter(|s| !s.nodes.is_empty()) {
            break status;
        }
        assert!(tokio::time::Instant::now() < deadline, "no per-node status within 180s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    for node in &status.nodes {
        println!("{}: verified={} writer={:?} reason={:?}", node.name, node.verified, node.writer_prefix, node.reason);
    }
    assert!(status.nodes.iter().all(|n| !n.name.is_empty() && !n.address.is_empty()));
    assert_eq!(status.rewrite.total, 0, "this test must never trigger a rewrite");
}
