use futures::StreamExt;
use kube::runtime::{predicates, reflector, watcher, Controller, Predicate, WatchStreamExt};
use kube::{Api, Client, Resource};
use platform_controller::ccm_reconciler;
use platform_controller::cert_manager::CertManagerInstallation;
use platform_controller::cert_manager_reconciler;
use platform_controller::cloud_controller_manager::CloudControllerManager;
use platform_controller::crd::CniInstallation;
use platform_controller::csi_driver::CsiDriver;
use platform_controller::csi_reconciler;
use platform_controller::leader;
use platform_controller::pull_through_cache::PullThroughCache;
use platform_controller::reconciler::{error_policy, reconcile_with_finalizer, Context};
use platform_controller::cache_reconciler;
use platform_controller::snapshot_controller::SnapshotController;
use platform_controller::snapshot_controller_reconciler;
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
fn deletion_requested<K: Resource>(obj: &K) -> Option<u64> {
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
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let controller = Controller::for_stream(installations, reader)
        .run(reconcile_with_finalizer, error_policy, context.clone())
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(err) => tracing::error!(error = %err, "reconcile failed"),
            }
        });

    // The pull-through cache gets its own watcher, store and Controller, but the
    // same status-write/deletion predicate filter (see above) and the same
    // Context, so both loops share one leader lease.
    let cache_api: Api<PullThroughCache> = Api::all(client.clone());
    let (cache_reader, cache_writer) = reflector::store();
    let caches = watcher(cache_api, watcher::Config::default())
        .default_backoff()
        .reflect(cache_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let cache_controller = Controller::for_stream(caches, cache_reader)
        .run(
            cache_reconciler::reconcile_with_finalizer,
            cache_reconciler::error_policy,
            context.clone(),
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled pull-through cache"),
                Err(err) => tracing::error!(error = %err, "pull-through cache reconcile failed"),
            }
        });

    // The cloud controller manager gets its own watcher, store and Controller
    // too, with the same predicate filter and the same Context (one leader lease).
    let ccm_api: Api<CloudControllerManager> = Api::all(client.clone());
    let (ccm_reader, ccm_writer) = reflector::store();
    let managers = watcher(ccm_api, watcher::Config::default())
        .default_backoff()
        .reflect(ccm_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let ccm_controller = Controller::for_stream(managers, ccm_reader)
        .run(
            ccm_reconciler::reconcile_with_finalizer,
            ccm_reconciler::error_policy,
            context.clone(),
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled cloud controller manager"),
                Err(err) => tracing::error!(error = %err, "cloud controller manager reconcile failed"),
            }
        });

    // The CSI driver component gets its own watcher, store and Controller too,
    // with the same predicate filter and the same Context (one leader lease).
    let csi_api: Api<CsiDriver> = Api::all(client.clone());
    let (csi_reader, csi_writer) = reflector::store();
    let drivers = watcher(csi_api, watcher::Config::default())
        .default_backoff()
        .reflect(csi_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let csi_controller = Controller::for_stream(drivers, csi_reader)
        .run(
            csi_reconciler::reconcile_with_finalizer,
            csi_reconciler::error_policy,
            context.clone(),
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled csi driver"),
                Err(err) => tracing::error!(error = %err, "csi driver reconcile failed"),
            }
        });

    // The cert-manager component gets its own watcher, store and Controller
    // too, with the same predicate filter and the same Context (one leader lease).
    let cert_manager_api: Api<CertManagerInstallation> = Api::all(client.clone());
    let (cert_manager_reader, cert_manager_writer) = reflector::store();
    let cert_manager_installations = watcher(cert_manager_api, watcher::Config::default())
        .default_backoff()
        .reflect(cert_manager_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let cert_manager_controller = Controller::for_stream(cert_manager_installations, cert_manager_reader)
        .run(
            cert_manager_reconciler::reconcile_with_finalizer,
            cert_manager_reconciler::error_policy,
            context.clone(),
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled cert-manager installation"),
                Err(err) => tracing::error!(error = %err, "cert-manager installation reconcile failed"),
            }
        });

    // The snapshot-controller component gets its own watcher, store and
    // Controller too, with the same predicate filter and the same Context
    // (one leader lease).
    let snapshot_controller_api: Api<SnapshotController> = Api::all(client.clone());
    let (snapshot_controller_reader, snapshot_controller_writer) = reflector::store();
    let snapshot_controllers = watcher(snapshot_controller_api, watcher::Config::default())
        .default_backoff()
        .reflect(snapshot_controller_writer)
        .applied_objects()
        .predicate_filter(
            predicates::generation
                .combine(deletion_requested)
                .combine(predicates::finalizers),
            Default::default(),
        );

    let snapshot_controller_controller = Controller::for_stream(snapshot_controllers, snapshot_controller_reader)
        .run(
            snapshot_controller_reconciler::reconcile_with_finalizer,
            snapshot_controller_reconciler::error_policy,
            context,
        )
        .for_each(|result| async move {
            match result {
                Ok(action) => tracing::debug!(?action, "reconciled snapshot controller"),
                Err(err) => tracing::error!(error = %err, "snapshot controller reconcile failed"),
            }
        });

    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = controller => {}
        _ = cache_controller => {}
        _ = ccm_controller => {}
        _ = csi_controller => {}
        _ = cert_manager_controller => {}
        _ = snapshot_controller_controller => {}
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

    #[test]
    fn deletion_requested_works_for_the_pull_through_cache_kind_too() {
        let cache: PullThroughCache = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "PullThroughCache",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-25T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "spegel",
                "spegel": { "chartVersion": "0.7.4" }
            }
        }))
        .expect("cache should deserialize");

        assert_eq!(deletion_requested(&cache), Some(1));
    }

    #[test]
    fn deletion_requested_works_for_the_cloud_controller_manager_kind_too() {
        let ccm: CloudControllerManager = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CloudControllerManager",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-26T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "provider": "openstack",
                "openstack": {
                    "chartVersion": "2.36.5",
                    "cloudConfigSecretRef": { "name": "cloud-config" }
                }
            }
        }))
        .expect("ccm should deserialize");

        assert_eq!(deletion_requested(&ccm), Some(1));
    }

    #[test]
    fn deletion_requested_works_for_the_csi_driver_kind_too() {
        let driver: CsiDriver = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CsiDriver",
            "metadata": {
                "name": "openstack-cinder",
                "deletionTimestamp": "2026-09-28T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "driver": "openstackCinder",
                "openstackCinder": {
                    "chartVersion": "2.36.5",
                    "cloudConfigSecretRef": { "name": "cloud-config" }
                }
            }
        }))
        .expect("driver should deserialize");

        assert_eq!(deletion_requested(&driver), Some(1));
    }

    #[test]
    fn deletion_requested_works_for_the_cert_manager_installation_kind_too() {
        let installation: CertManagerInstallation = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "CertManagerInstallation",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-28T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "chartVersion": "v1.16.2"
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), Some(1));
    }

    #[test]
    fn deletion_requested_works_for_the_snapshot_controller_kind_too() {
        let installation: SnapshotController = serde_json::from_value(serde_json::json!({
            "apiVersion": "platform.rye.ninja/v1alpha1",
            "kind": "SnapshotController",
            "metadata": {
                "name": "default",
                "deletionTimestamp": "2026-09-29T00:00:00Z"
            },
            "spec": {
                "platformKind": "talos-linux",
                "chartVersion": "5.3.0"
            }
        }))
        .expect("installation should deserialize");

        assert_eq!(deletion_requested(&installation), Some(1));
    }
}
