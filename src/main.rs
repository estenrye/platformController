use futures::StreamExt;
use kube::runtime::{watcher, Controller};
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

    Controller::new(api, watcher::Config::default())
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
