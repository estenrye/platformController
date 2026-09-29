// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running, all six CRDs Established, and CertManagerInstallation
// already Ready (this component has a hard dependency on it -- see
// examples/snapshot-controller.yaml):
//
//   kubectl apply -f examples/cert-manager.yaml
//   kubectl wait --for=jsonpath='{.status.phase}'=Ready certmgr/default --timeout=300s
//   kubectl apply -f examples/snapshot-controller.yaml
//   cargo test --test integration_snapshot_controller -- --ignored --nocapture
//
// The test deletes the SnapshotController at the end, so re-apply the
// example to run it again. It does NOT exercise the Issuer/Certificate
// smoke test or a real VolumeSnapshot; those are manual runbook steps
// (docs/runbooks/snapshot-controller-verification.md).

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::crd::Phase;
use platform_controller::snapshot_controller::SnapshotController;
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
async fn snapshot_controller_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let installations: Api<SnapshotController> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "snapshot-controller");
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());

    eventually("SnapshotController reaching Ready", Duration::from_secs(300), || async {
        installations
            .get("default")
            .await
            .ok()
            .and_then(|installation| installation.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    let listed = deployments
        .list(&ListParams::default())
        .await
        .expect("should list Deployments in the snapshot-controller namespace");
    assert_eq!(
        listed.items.len(),
        2,
        "Ready but the expected 2 Deployments (controller, conversion-webhook) are not both present: {:?}",
        listed.items.iter().filter_map(|d| d.metadata.name.clone()).collect::<Vec<_>>()
    );

    installations
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the SnapshotController");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        installations.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the snapshot-controller namespace disappearing", Duration::from_secs(120), || async {
        namespaces.get_opt("snapshot-controller").await.expect("get_opt should succeed").is_none()
    })
    .await;
    // installCRDs: true put this CRD in the ledger too -- confirms the
    // cascade-delete behavior documented in deploy/README.md and the runbook
    // is real, not just a claim.
    eventually("the volumesnapshots CRD disappearing", Duration::from_secs(120), || async {
        crds.get_opt("volumesnapshots.snapshot.storage.k8s.io")
            .await
            .expect("get_opt should succeed")
            .is_none()
    })
    .await;
}
