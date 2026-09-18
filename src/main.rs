use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, WatchStreamExt};
use kube::{Api, Client};
use platform_controller::crd::CniInstallation;
use platform_controller::reconciler::{error_policy, reconcile, Context};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = Client::try_default().await?;
    tracing::info!(
        default_namespace = client.default_namespace(),
        "connected to kubernetes"
    );

    let api: Api<CniInstallation> = Api::all(client.clone());
    let context = Arc::new(Context { client });

    // The controller's own `update_status` call patches `status` on every
    // successful reconcile, which bumps `resourceVersion` and would otherwise
    // immediately re-trigger reconcile through the watch stream below,
    // starving the intended 300s periodic resync (`Action::requeue` in
    // `reconciler::reconcile`). Kubernetes only increments `metadata.generation`
    // on spec changes, not on status-subresource patches, so filtering the
    // watch stream on generation drops these status-only self-writes while
    // still passing through genuine spec changes.
    let (reader, writer) = reflector::store();
    let installations = watcher(api, watcher::Config::default())
        .default_backoff()
        .reflect(writer)
        .applied_objects()
        .predicate_filter(predicates::generation, Default::default());

    Controller::for_stream(installations, reader)
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        })
        .await;

    Ok(())
}
