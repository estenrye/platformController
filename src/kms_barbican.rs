use crate::etcd_encryption::BarbicanSpec;
use crate::kms_provider::{KmsPlan, KMS_SOCKET_DIR, PLUGIN_NAMESPACE};
use kube::api::DynamicObject;

const DAEMONSET_NAME: &str = "barbican-kms";

/// The Barbican KMS plugin as a DaemonSet on the control-plane nodes, built
/// from typed fields. Shape taken from upstream
/// `manifests/barbican-kms/ds.yaml` (2026-09-30), minus its
/// `serviceAccountName` (the plugin never calls the Kubernetes API) and plus
/// `dnsPolicy: Default` (no cluster-DNS dependence on the bootstrap path),
/// a socket `readinessProbe` and `priorityClassName: system-node-critical`.
pub fn plan(spec: &BarbicanSpec) -> KmsPlan {
    let daemonset: DynamicObject = serde_json::from_value(serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "DaemonSet",
        "metadata": {
            "name": DAEMONSET_NAME,
            "namespace": PLUGIN_NAMESPACE,
            "labels": { "k8s-app": DAEMONSET_NAME },
        },
        "spec": {
            "selector": { "matchLabels": { "k8s-app": DAEMONSET_NAME } },
            "updateStrategy": { "type": "RollingUpdate" },
            "template": {
                "metadata": { "labels": { "k8s-app": DAEMONSET_NAME } },
                "spec": {
                    "hostNetwork": true,
                    "dnsPolicy": "Default",
                    // The apiserver cannot read Secrets without this pod.
                    "priorityClassName": "system-node-critical",
                    "nodeSelector": { "node-role.kubernetes.io/control-plane": "" },
                    "tolerations": [
                        { "key": "node.cloudprovider.kubernetes.io/uninitialized", "operator": "Exists", "effect": "NoSchedule" },
                        { "key": "node-role.kubernetes.io/master", "effect": "NoSchedule" },
                        { "key": "node-role.kubernetes.io/control-plane", "effect": "NoSchedule" },
                    ],
                    "containers": [{
                        "name": DAEMONSET_NAME,
                        "image": spec.image,
                        "args": [
                            "/bin/barbican-kms-plugin",
                            "--socketpath=/kms/kms.sock",
                            "--cloud-config=/etc/config/cloud.conf",
                        ],
                        "volumeMounts": [
                            { "name": "cloud-config-volume", "mountPath": "/etc/config" },
                            { "name": "socket-dir", "mountPath": "/kms/" },
                        ],
                        "livenessProbe": {
                            "exec": { "command": ["ls", "/kms/kms.sock"] },
                            "failureThreshold": 5,
                            "initialDelaySeconds": 10,
                            "timeoutSeconds": 10,
                            "periodSeconds": 60,
                        },
                        // Ready (and so counted by plugin_ready) only once the
                        // socket the apiserver dials exists.
                        "readinessProbe": {
                            "exec": { "command": ["ls", "/kms/kms.sock"] },
                            "initialDelaySeconds": 5,
                            "periodSeconds": 10,
                            "failureThreshold": 3,
                        },
                    }],
                    "volumes": [
                        { "name": "cloud-config-volume", "secret": { "secretName": spec.cloud_config_secret_ref.name } },
                        { "name": "socket-dir", "hostPath": { "path": format!("{KMS_SOCKET_DIR}/"), "type": "DirectoryOrCreate" } },
                    ],
                },
            },
        },
    }))
    .expect("static DaemonSet JSON deserializes into a DynamicObject");

    KmsPlan {
        provider_name: "barbican",
        socket_path: format!("{KMS_SOCKET_DIR}/kms.sock"),
        daemonset_name: DAEMONSET_NAME.to_string(),
        daemonset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etcd_encryption::SecretNameRef;

    fn spec() -> BarbicanSpec {
        BarbicanSpec {
            image: "registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0".to_string(),
            cloud_config_secret_ref: SecretNameRef { name: "barbican-kms-cloud-config".to_string() },
        }
    }

    #[test]
    fn plan_names_the_socket_and_daemonset() {
        let plan = plan(&spec());

        assert_eq!(plan.provider_name, "barbican");
        assert_eq!(plan.socket_path, "/var/lib/kms/kms.sock");
        assert_eq!(plan.daemonset_name, "barbican-kms");
    }

    #[test]
    fn daemonset_is_a_hostnetwork_control_plane_daemonset_in_kube_system() {
        let object = plan(&spec()).daemonset;
        let types = object.types.as_ref().unwrap();

        assert_eq!((types.api_version.as_str(), types.kind.as_str()), ("apps/v1", "DaemonSet"));
        assert_eq!(object.metadata.name.as_deref(), Some("barbican-kms"));
        assert_eq!(object.metadata.namespace.as_deref(), Some("kube-system"));
        let pod = &object.data["spec"]["template"]["spec"];
        assert_eq!(pod["hostNetwork"], true);
        assert_eq!(pod["dnsPolicy"], "Default");
        assert_eq!(pod["nodeSelector"]["node-role.kubernetes.io/control-plane"], "");
        assert!(pod.get("serviceAccountName").is_none());
    }

    #[test]
    fn daemonset_tolerates_control_plane_and_uninitialized_taints() {
        let object = plan(&spec()).daemonset;
        let tolerations = object.data["spec"]["template"]["spec"]["tolerations"].as_array().unwrap();
        let keys: Vec<&str> = tolerations.iter().map(|t| t["key"].as_str().unwrap()).collect();

        assert!(keys.contains(&"node-role.kubernetes.io/control-plane"));
        assert!(keys.contains(&"node-role.kubernetes.io/master"));
        assert!(keys.contains(&"node.cloudprovider.kubernetes.io/uninitialized"));
    }

    #[test]
    fn container_uses_the_pinned_image_and_upstream_args() {
        let object = plan(&spec()).daemonset;
        let container = &object.data["spec"]["template"]["spec"]["containers"][0];

        assert_eq!(container["image"], "registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0");
        assert_eq!(
            container["args"],
            serde_json::json!([
                "/bin/barbican-kms-plugin",
                "--socketpath=/kms/kms.sock",
                "--cloud-config=/etc/config/cloud.conf"
            ])
        );
    }

    #[test]
    fn volumes_mount_the_credentials_secret_and_the_host_socket_dir() {
        let object = plan(&spec()).daemonset;
        let volumes = object.data["spec"]["template"]["spec"]["volumes"].as_array().unwrap();

        let secret = volumes.iter().find(|v| v["name"] == "cloud-config-volume").unwrap();
        assert_eq!(secret["secret"]["secretName"], "barbican-kms-cloud-config");
        let socket = volumes.iter().find(|v| v["name"] == "socket-dir").unwrap();
        assert_eq!(socket["hostPath"]["path"], "/var/lib/kms/");
        assert_eq!(socket["hostPath"]["type"], "DirectoryOrCreate");
    }

    #[test]
    fn the_plugin_is_ready_only_once_its_socket_exists() {
        // plugin_ready() counts Ready pods: without a readinessProbe a pod is
        // Ready before the socket the apiserver needs is there.
        let object = plan(&spec()).daemonset;
        let probe = &object.data["spec"]["template"]["spec"]["containers"][0]["readinessProbe"];

        assert_eq!(probe["exec"]["command"], serde_json::json!(["ls", "/kms/kms.sock"]));
        assert_eq!(probe["periodSeconds"], 10);
        assert_eq!(probe["initialDelaySeconds"], 5);
        assert_eq!(probe["failureThreshold"], 3);
    }

    #[test]
    fn the_plugin_runs_at_system_node_critical_priority() {
        // The apiserver cannot read Secrets without it: it must not be
        // preempted or evicted ahead of ordinary workloads.
        let object = plan(&spec()).daemonset;

        assert_eq!(object.data["spec"]["template"]["spec"]["priorityClassName"], "system-node-critical");
    }
}
