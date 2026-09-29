// Run manually against a real cluster (the Talos-in-Docker setup in
// tests/integration_talos.rs is enough; no OpenStack is needed). With the
// controller running and all five CRDs Established, and CniInstallation
// already Ready:
//
//   kubectl apply -f examples/cert-manager.yaml
//   cargo test --test integration_cert_manager -- --ignored --nocapture
//
// The test deletes the CertManagerInstallation at the end, so re-apply the
// example to run it again. It does NOT configure any ClusterIssuer/Certificate
// smoke test; that is a manual runbook step
// (docs/runbooks/cert-manager-verification.md).

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::cert_manager::CertManagerInstallation;
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
async fn cert_manager_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let installations: Api<CertManagerInstallation> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "cert-manager");

    eventually("CertManagerInstallation reaching Ready", Duration::from_secs(300), || async {
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
        .expect("should list Deployments in the cert-manager namespace");
    assert_eq!(
        listed.items.len(),
        3,
        "Ready but the expected 3 Deployments (controller, webhook, cainjector) are not all present: {:?}",
        listed.items.iter().filter_map(|d| d.metadata.name.clone()).collect::<Vec<_>>()
    );

    installations
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the CertManagerInstallation");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        installations.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the cert-manager namespace disappearing", Duration::from_secs(120), || async {
        namespaces.get_opt("cert-manager").await.expect("get_opt should succeed").is_none()
    })
    .await;
}
