// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running and all three CRDs Established:
//
//   kubectl apply -f examples/cloud-controller-manager.yaml
//   cargo test --test integration_cloud_controller_manager -- --ignored --nocapture
//
// The test deletes the CloudControllerManager at the end, so re-apply the example
// to run it again. It does NOT assert that the CCM pod runs: without the
// cloud-config Secret and an OpenStack to talk to, the pod cannot start. That is
// a manual runbook step (docs/runbooks/cloud-controller-manager-verification.md).

use k8s_openapi::api::apps::v1::DaemonSet;
use kube::api::{Api, DeleteParams};
use kube::Client;
use platform_controller::cloud_controller_manager::CloudControllerManager;
use platform_controller::crd::Phase;
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} did not happen within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
#[ignore = "requires a real cluster with the controller running; see module docs"]
async fn openstack_ccm_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let managers: Api<CloudControllerManager> = Api::all(client.clone());
    let daemon_sets: Api<DaemonSet> = Api::namespaced(client.clone(), "kube-system");

    eventually("CloudControllerManager reaching Ready", Duration::from_secs(300), || async {
        managers
            .get("default")
            .await
            .ok()
            .and_then(|manager| manager.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    assert!(
        daemon_sets
            .get_opt("openstack-cloud-controller-manager")
            .await
            .expect("get_opt should succeed")
            .is_some(),
        "Ready but the DaemonSet does not exist in kube-system"
    );

    managers
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the CloudControllerManager");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        managers.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the DaemonSet disappearing", Duration::from_secs(120), || async {
        daemon_sets
            .get_opt("openstack-cloud-controller-manager")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
}
