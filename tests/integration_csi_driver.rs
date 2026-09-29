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

// Run manually against a real cluster with a CsiDriver already applied and
// Ready (this test mutates an existing one; it doesn't create it from
// scratch, since csi-cinder-sc-delete must already exist for the
// immutable-field conflict to trigger at all):
//
//   kubectl apply -f examples/csi-driver-openstack-cinder.yaml
//   # wait for it to reach Ready, then:
//   cargo test --test integration_csi_driver openstack_cinder_csi_recovers -- --ignored --nocapture
//
// Leaves the CsiDriver's csi-cinder-sc-delete/retain parameters set to
// {availability: nova} at the end -- the real fix for the Nova/Cinder
// availability-zone mismatch this environment hits, not just a test marker.
#[tokio::test]
#[ignore = "requires a real cluster with the controller running and a Ready CsiDriver; see module docs"]
async fn openstack_cinder_csi_recovers_from_an_immutable_storage_class_parameter_change() {
    use k8s_openapi::api::storage::v1::StorageClass;
    use kube::api::{Patch, PatchParams};

    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the test cluster");

    let drivers: Api<CsiDriver> = Api::all(client.clone());
    let storage_classes: Api<StorageClass> = Api::all(client.clone());

    eventually(
        "CsiDriver already Ready before this test begins",
        Duration::from_secs(60),
        || async {
            drivers
                .get("openstack-cinder")
                .await
                .ok()
                .and_then(|driver| driver.status)
                .is_some_and(|status| matches!(status.phase, Phase::Ready))
        },
    )
    .await;

    let before = storage_classes
        .get("csi-cinder-sc-delete")
        .await
        .expect("csi-cinder-sc-delete should already exist");
    let before_uid = before.metadata.uid.clone();

    // StorageClass.parameters is immutable on update, so this must fail to
    // apply in place and exercise the reconciler's delete-and-recreate
    // recovery -- no manual kubectl intervention in this test.
    let marker_patch = serde_json::json!({
        "spec": { "openstackCinder": { "storageClasses": {
            "delete": { "parameters": { "availability": "integration-test-marker" } },
            "retain": { "parameters": { "availability": "integration-test-marker" } }
        } } }
    });
    let patched = drivers
        .patch("openstack-cinder", &PatchParams::default(), &Patch::Merge(&marker_patch))
        .await
        .expect("should patch the CsiDriver with the marker value");
    let marker_generation = patched.metadata.generation;

    // status.phase can still read Ready from *before* this patch until the
    // reconciler actually processes the new generation, so check
    // observedGeneration too -- otherwise this races ahead on stale status.
    eventually(
        "CsiDriver reaching Ready again, at the new generation, after the immutable-field recovery",
        Duration::from_secs(120),
        || async {
            drivers
                .get("openstack-cinder")
                .await
                .ok()
                .and_then(|driver| driver.status)
                .is_some_and(|status| {
                    matches!(status.phase, Phase::Ready)
                        && Some(status.observed_generation) == marker_generation
                })
        },
    )
    .await;

    let after = storage_classes
        .get("csi-cinder-sc-delete")
        .await
        .expect("csi-cinder-sc-delete should exist again after recovery");
    assert_eq!(
        after.parameters.as_ref().and_then(|p| p.get("availability")).map(String::as_str),
        Some("integration-test-marker"),
        "the new parameters value should have taken effect"
    );
    assert_ne!(
        after.metadata.uid, before_uid,
        "the StorageClass should have been deleted and recreated (a new UID), not left as-is"
    );

    // Leave the cluster in the real desired state, not the test marker.
    let nova_patch = serde_json::json!({
        "spec": { "openstackCinder": { "storageClasses": {
            "delete": { "parameters": { "availability": "nova" } },
            "retain": { "parameters": { "availability": "nova" } }
        } } }
    });
    let restored = drivers
        .patch("openstack-cinder", &PatchParams::default(), &Patch::Merge(&nova_patch))
        .await
        .expect("should patch the CsiDriver back to the real availability zone");
    let nova_generation = restored.metadata.generation;

    eventually(
        "CsiDriver reaching Ready, at the new generation, after restoring the real availability zone",
        Duration::from_secs(120),
        || async {
            drivers
                .get("openstack-cinder")
                .await
                .ok()
                .and_then(|driver| driver.status)
                .is_some_and(|status| {
                    matches!(status.phase, Phase::Ready) && Some(status.observed_generation) == nova_generation
                })
        },
    )
    .await;

    let final_state = storage_classes
        .get("csi-cinder-sc-delete")
        .await
        .expect("csi-cinder-sc-delete should exist after restoring nova");
    assert_eq!(
        final_state.parameters.as_ref().and_then(|p| p.get("availability")).map(String::as_str),
        Some("nova"),
        "the cluster should end this test back on the real availability zone, not the marker"
    );
}
