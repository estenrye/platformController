use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, Predicate, WatchStreamExt};
use kube::{Api, Client, Resource};
use platform_controller::crd::CniInstallation;
use platform_controller::leader;
use platform_controller::reconciler::{error_policy, reconcile_with_finalizer, Context};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

const LEASE_NAMESPACE: &str = "platform-system";
const LEASE_NAME: &str = "platform-controller-leader";

/// `predicates::generation` alone misses deletions: `kubectl delete` on an
/// object with a finalizer only sets `metadata.deletionTimestamp`, which,
/// like a status-subresource write, does not bump `metadata.generation`
/// (verified against `kube-runtime` 4.2.0's own source). Combined below with
/// `predicates::generation` so both spec changes and deletion requests pass
/// through the filter, while status-only self-writes still don't.
fn deletion_requested(obj: &CniInstallation) -> Option<u64> {
    obj.meta().deletion_timestamp.is_some().then_some(1)
}

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
    // Supervised below in the `select!`, not fire-and-forget: `tokio::spawn`
    // swallows panics, and `leader::run` never returns normally, so a panic
    // would otherwise latch `is_leader` at its last value forever with no log
    // signal — a phantom leader or a phantom standby.
    let mut leader_handle = tokio::spawn(leader::run(
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
        .predicate_filter(predicates::generation.combine(deletion_requested), Default::default());

    let controller = Controller::for_stream(installations, reader)
        .run(reconcile_with_finalizer, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        });

    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = controller => {}
        // `leader::run` loops forever, so this branch only resolves if it
        // panicked. Exit non-zero and let Kubernetes restart the pod rather than
        // limp on with a permanently stale `is_leader` flag.
        join_result = &mut leader_handle => {
            tracing::error!(?join_result, "leader-election task exited unexpectedly");
            return Err(anyhow::anyhow!(
                "leader-election task exited unexpectedly: {join_result:?}"
            ));
        }
        // SIGTERM wins this race by *dropping* the Controller future, which
        // aborts any in-flight reconcile wherever it happened to be — unlike
        // lease loss, which only gates the *next* reconcile. That is safe
        // because the successor leader's next reconcile re-applies the full
        // resource set tracked in `status.appliedResources`, converging no
        // matter how far the interrupted reconcile got.
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, releasing lease if held");
            // Stop the renewal loop before releasing: otherwise its next
            // `get_opt` could land after `release`'s write, still see itself as
            // holder, and silently re-renew — undoing the graceful handoff.
            leader_handle.abort();
            leader::release(client, LEASE_NAMESPACE.to_string(), LEASE_NAME.to_string(), identity).await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_requested_is_none_without_a_deletion_timestamp() {
        let installation: CniInstallation = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CniInstallation",
            "metadata": { "name": "default" },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "calico",
                "calico": { "chartVersion": "v3.29.1" }
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), None);
    }

    #[test]
    fn deletion_requested_is_some_once_deletion_timestamp_is_set() {
        let installation: CniInstallation = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CniInstallation",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-19T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "calico",
                "calico": { "chartVersion": "v3.29.1" }
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), Some(1));
    }
}
