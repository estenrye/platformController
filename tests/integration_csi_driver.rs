// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running and all four CRDs Established:
//
//   kubectl apply -f examples/csi-driver-openstack-cinder.yaml
//   cargo test --test integration_csi_driver -- --ignored --nocapture
//
// The test deletes the CsiDriver at the end, so re-apply the example to run it
// again. It does NOT assert that the driver pods run: without the cloud-config
// Secret and an OpenStack to talk to, they cannot start. That is a manual
// runbook step (docs/runbooks/csi-driver-openstack-cinder-verification.md).

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use kube::api::{Api, DeleteParams};
use kube::Client;
use platform_controller::crd::Phase;
use platform_controller::csi_driver::CsiDriver;
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
async fn openstack_cinder_csi_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let drivers: Api<CsiDriver> = Api::all(client.clone());
    let daemon_sets: Api<DaemonSet> = Api::namespaced(client.clone(), "kube-system");
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    eventually("CsiDriver reaching Ready", Duration::from_secs(300), || async {
        drivers
            .get("openstack-cinder")
            .await
            .ok()
            .and_then(|driver| driver.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    assert!(
        daemon_sets
            .get_opt("openstack-cinder-csi-nodeplugin")
            .await
            .expect("get_opt should succeed")
            .is_some(),
        "Ready but the node plugin DaemonSet does not exist in kube-system"
    );
    assert!(
        deployments
            .get_opt("openstack-cinder-csi-controllerplugin")
            .await
            .expect("get_opt should succeed")
            .is_some(),
        "Ready but the controller plugin Deployment does not exist in kube-system"
    );

    drivers
        .delete("openstack-cinder", &DeleteParams::default())
        .await
        .expect("should delete the CsiDriver");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        drivers.get_opt("openstack-cinder").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the node plugin DaemonSet disappearing", Duration::from_secs(120), || async {
        daemon_sets
            .get_opt("openstack-cinder-csi-nodeplugin")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
    eventually("the controller plugin Deployment disappearing", Duration::from_secs(120), || async {
        deployments
            .get_opt("openstack-cinder-csi-controllerplugin")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
}
