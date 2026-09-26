// Run manually against a real Talos cluster whose nodes already have the Spegel
// machine-config prerequisite and a working CNI. The setup is the one in
// tests/integration_talos.rs, with `--workers 2`, plus
// docs/runbooks/pull-through-cache-verification.md step 0. With the controller
// running and both CRDs Established:
//
//   kubectl apply -f examples/pull-through-cache.yaml
//   cargo test --test integration_pull_through_cache -- --ignored --nocapture
//
// The test deletes the PullThroughCache at the end, so re-apply the example to
// run it again. It does not try to prove peer-to-peer serving; that is a manual
// runbook step.

use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;
use platform_controller::crd::Phase;
use platform_controller::pull_through_cache::PullThroughCache;
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
#[ignore = "requires a real Talos cluster with a CNI and the Spegel prerequisite; see module docs"]
async fn spegel_is_applied_and_cleaned_up() {
    let client = Client::try_default()
        .await
        .expect("KUBECONFIG should point at the Talos test cluster");

    let caches: Api<PullThroughCache> = Api::all(client.clone());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let daemon_sets: Api<DaemonSet> = Api::namespaced(client.clone(), "spegel");

    eventually("PullThroughCache reaching Ready", Duration::from_secs(300), || async {
        caches
            .get("default")
            .await
            .ok()
            .and_then(|cache| cache.status)
            .is_some_and(|status| matches!(status.phase, Phase::Ready))
    })
    .await;

    let listed = daemon_sets
        .list(&ListParams::default())
        .await
        .expect("should list DaemonSets in the spegel namespace");
    assert!(
        !listed.items.is_empty(),
        "Ready but no DaemonSet exists in the spegel namespace"
    );

    caches
        .delete("default", &DeleteParams::default())
        .await
        .expect("should delete the PullThroughCache");

    eventually("the finalizer clearing", Duration::from_secs(120), || async {
        caches.get_opt("default").await.expect("get_opt should succeed").is_none()
    })
    .await;
    eventually("the spegel namespace disappearing", Duration::from_secs(180), || async {
        namespaces.get_opt("spegel").await.expect("get_opt should succeed").is_none()
    })
    .await;
}
