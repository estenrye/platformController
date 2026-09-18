use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, WatchStreamExt};
use kube::{Api, Client};
use platform_controller::crd::CniInstallation;
use platform_controller::leader;
use platform_controller::reconciler::{error_policy, reconcile, Context};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

const LEASE_NAMESPACE: &str = "platform-system";
const LEASE_NAME: &str = "platform-controller-leader";

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

    let identity = std::env::var("POD_NAME")
        .unwrap_or_else(|_| format!("platform-controller-{}", std::process::id()));

    let is_leader = Arc::new(AtomicBool::new(false));
    tokio::spawn(leader::run(
        client.clone(),
        LEASE_NAMESPACE.to_string(),
        LEASE_NAME.to_string(),
        identity.clone(),
        is_leader.clone(),
    ));

    let api: Api<CniInstallation> = Api::all(client.clone());
    let context = Arc::new(Context {
        client: client.clone(),
        is_leader,
    });

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

    let controller = Controller::for_stream(installations, reader)
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        });

    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = controller => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, releasing lease if held");
            leader::release(client, LEASE_NAMESPACE.to_string(), LEASE_NAME.to_string(), identity).await;
        }
    }

    Ok(())
}
