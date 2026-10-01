// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough for this narrow check; no OpenStack is
// needed for it to reach AwaitingKmsConfig only if the plugin pods can start,
// so on a cluster without Barbican use the runbook instead). With the
// controller running and all seven CRDs Established:
//
//   kubectl apply -f examples/etcd-encryption.yaml
//   cargo test --test integration_etcd_encryption -- --ignored --nocapture
//
// This checks only what the controller can do by itself: install the plugin
// DaemonSet and publish patch 1. It does NOT apply any Talos patch; the rest
// of the protocol is a manual runbook step
// (docs/runbooks/etcd-encryption-verification.md). It deletes the resource at
// the end, which, with no acknowledgement set, removes the plugin immediately.

use k8s_openapi::api::apps::v1::DaemonSet;
use kube::api::{Api, DeleteParams};
use kube::Client;
use platform_controller::etcd_encryption::{EncryptionPhase, EtcdEncryption};
use std::time::Duration;

async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what} did not happen within {timeout:?}");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
#[ignore = "needs a live cluster with the controller running; see the header comment"]
async fn installs_the_plugin_and_publishes_the_enable_kms_patch() {
    let client = Client::try_default().await.expect("a kubeconfig for the test cluster");
    let encryptions: Api<EtcdEncryption> = Api::all(client.clone());
    let daemonsets: Api<DaemonSet> = Api::namespaced(client.clone(), "kube-system");

    eventually("the barbican-kms DaemonSet to exist", Duration::from_secs(120), || async {
        daemonsets.get_opt("barbican-kms").await.unwrap().is_some()
    })
    .await;

    eventually("the resource to reach AwaitingKmsConfig", Duration::from_secs(300), || async {
        encryptions
            .get("default")
            .await
            .ok()
            .and_then(|e| e.status)
            .is_some_and(|s| s.phase == EncryptionPhase::AwaitingKmsConfig)
    })
    .await;

    let status = encryptions.get("default").await.unwrap().status.unwrap();
    let patch = status.talos_patches.enable_kms.expect("patch 1 is published");
    assert!(patch.contains("KubeEtcdEncryptionConfig"), "{patch}");

    encryptions.delete("default", &DeleteParams::default()).await.unwrap();
    eventually("the plugin to be removed on delete", Duration::from_secs(120), || async {
        daemonsets.get_opt("barbican-kms").await.unwrap().is_none()
    })
    .await;
}
