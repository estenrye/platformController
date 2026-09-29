use crate::crd::{CalicoSpec, Encapsulation, NodeAddressAutodetectionMethod};

/// Namespace the tigera-operator chart's namespaced objects belong in. The chart
/// itself renders no `Namespace` object, so the controller both renders into and
/// creates this namespace explicitly.
pub const TIGERA_OPERATOR_NAMESPACE: &str = "tigera-operator";

/// The Helm repository the tigera-operator chart is fetched from.
pub const CALICO_CHART_REPO: &str = "https://docs.tigera.io/calico/charts";

/// Namespace the Spegel chart's namespaced objects belong in. Like
/// tigera-operator, the chart renders no `Namespace` object of its own.
pub const SPEGEL_NAMESPACE: &str = "spegel";

/// Namespace the OpenStack cloud-controller-manager chart is released into. The
/// namespace already exists and Talos's default admission configuration exempts
/// it from Pod Security, so unlike Spegel no namespace is synthesized.
pub const CCM_NAMESPACE: &str = "kube-system";

/// The Helm repository both OpenStack charts (cloud-controller-manager and
/// cinder-csi) are fetched from.
pub const CLOUD_PROVIDER_OPENSTACK_CHART_REPO: &str = "https://kubernetes.github.io/cloud-provider-openstack";

/// Namespace the openstack-cinder-csi chart is released into. Same reasoning
/// as `CCM_NAMESPACE`: the chart mounts the cloud-config Secret from its own
/// release namespace, and `kube-system` already exists on Talos, so no
/// namespace is synthesized.
pub const CINDER_CSI_NAMESPACE: &str = "kube-system";

/// Namespace the cert-manager chart's namespaced objects belong in. Like
/// tigera-operator and Spegel, the chart renders no `Namespace` object of its
/// own (live-verified against chart v1.16.2).
pub const CERT_MANAGER_NAMESPACE: &str = "cert-manager";

/// Where a Helm chart is fetched from. The two forms invoke `helm template`
/// differently.
pub enum ChartSource {
    /// `helm template <release> --repo <url> <chart>`
    Repo { url: &'static str, chart: &'static str },
    /// `helm template <release> <reference>`, where the reference is `oci://...`
    Oci { reference: &'static str },
}

/// Everything about a chart except its version and values.
pub struct ChartRef {
    pub release: &'static str,
    pub source: ChartSource,
    pub namespace: &'static str,
}

pub const CALICO_CHART: ChartRef = ChartRef {
    release: "calico",
    source: ChartSource::Repo {
        url: CALICO_CHART_REPO,
        chart: "tigera-operator",
    },
    namespace: TIGERA_OPERATOR_NAMESPACE,
};

pub const SPEGEL_CHART: ChartRef = ChartRef {
    release: "spegel",
    source: ChartSource::Oci {
        reference: "oci://ghcr.io/spegel-org/helm-charts/spegel",
    },
    namespace: SPEGEL_NAMESPACE,
};

/// The release name is part of the DaemonSet's immutable `selector` (the chart
/// labels pods `release: <name>`), so it must never change for an installed cluster.
pub const OPENSTACK_CCM_CHART: ChartRef = ChartRef {
    release: "openstack-ccm",
    source: ChartSource::Repo {
        url: CLOUD_PROVIDER_OPENSTACK_CHART_REPO,
        chart: "openstack-cloud-controller-manager",
    },
    namespace: CCM_NAMESPACE,
};

/// The release name is part of the node DaemonSet's and controller
/// Deployment's immutable `selector` (the chart labels pods `release:
/// <name>`), so it must never change for an installed cluster.
pub const OPENSTACK_CINDER_CSI_CHART: ChartRef = ChartRef {
    release: "cinder-csi",
    source: ChartSource::Repo {
        url: CLOUD_PROVIDER_OPENSTACK_CHART_REPO,
        chart: "openstack-cinder-csi",
    },
    namespace: CINDER_CSI_NAMESPACE,
};

/// The OCI registry cert-manager's chart is published to -- Jetstack's
/// current recommended distribution channel. The classic
/// `https://charts.jetstack.io` repo is deprecated.
pub const CERT_MANAGER_CHART: ChartRef = ChartRef {
    release: "cert-manager",
    source: ChartSource::Oci {
        reference: "oci://quay.io/jetstack/charts/cert-manager",
    },
    namespace: CERT_MANAGER_NAMESPACE,
};

pub fn build_values(calico: &CalicoSpec) -> serde_json::Value {
    let ip_pools: Vec<serde_json::Value> = calico
        .ip_pools
        .iter()
        // A retired pool is still applied as a disabled IPPool by crate::calico,
        // but must not be declared here: the operator would recreate it enabled.
        .filter(|pool| !pool.disabled)
        .map(|pool| {
            serde_json::json!({
                // The operator only adopts a pre-existing explicit IPPool
                // (rendered by crate::calico) instead of creating a second one
                // when both carry the same name.
                "name": pool.name,
                "cidr": pool.cidr,
                "encapsulation": encapsulation_str(&pool.encapsulation),
                "natOutgoing": bool_to_enum(pool.nat_outgoing),
                "blockSize": pool.effective_block_size(),
                "nodeSelector": pool.node_selector,
            })
        })
        .collect();

    let mut calico_network = serde_json::json!({
        "bgp": bool_to_enum(calico.bgp_enabled),
        "ipPools": ip_pools,
    });

    // The operator rejects `nodeAddressAutodetectionV6: {cidrs: []}` (an
    // autodetection method with no method selected), so omit the key entirely
    // when no method is configured. Calico accepts only one method at a time;
    // `spec_validation` rejects a CIDR list alongside `kubernetesInternalIP`.
    match calico.node_address_autodetection_v6_method {
        NodeAddressAutodetectionMethod::KubernetesInternalIp => {
            calico_network["nodeAddressAutodetectionV6"] =
                serde_json::json!({ "kubernetes": "NodeInternalIP" });
        }
        NodeAddressAutodetectionMethod::Cidrs => {
            if !calico.node_address_autodetection_v6_cidrs.is_empty() {
                calico_network["nodeAddressAutodetectionV6"] = serde_json::json!({
                    "cidrs": calico.node_address_autodetection_v6_cidrs,
                });
            }
        }
    }

    serde_json::json!({
        "installation": {
            "enabled": true,
            // Talos's root filesystem is read-only, so the legacy FlexVolume
            // driver's init container (which needs to mkdir under
            // /usr/libexec/kubernetes) always crash-loops. FlexVolume is
            // superseded by CSI (already deployed via csi-node-driver), and
            // this controller only ever targets talos-linux today, so
            // disabling it unconditionally is correct, not a platform-specific
            // workaround bolted onto a shared default.
            "flexVolumePath": "None",
            "calicoNetwork": calico_network,
        },
        "apiServer": {
            "enabled": calico.api_server_enabled,
        },
    })
}

fn bool_to_enum(value: bool) -> &'static str {
    if value {
        "Enabled"
    } else {
        "Disabled"
    }
}

fn encapsulation_str(encapsulation: &Encapsulation) -> &'static str {
    match encapsulation {
        Encapsulation::Ipip => "IPIP",
        Encapsulation::Vxlan => "VXLAN",
        Encapsulation::None => "None",
    }
}

#[derive(thiserror::Error, Debug)]
pub enum HelmError {
    #[error("failed to write helm values file: {0}")]
    WriteValues(#[source] std::io::Error),
    #[error("failed to launch helm: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("helm exited with status {status}: {stderr}")]
    NonZeroExit {
        status: std::process::ExitStatus,
        stderr: String,
    },
}

pub fn render_args(chart: &ChartRef, chart_version: &str, values_path: &std::path::Path) -> Vec<String> {
    let mut args = vec!["template".to_string(), chart.release.to_string()];
    match &chart.source {
        ChartSource::Repo { url, chart: name } => {
            args.extend(["--repo".to_string(), url.to_string(), name.to_string()]);
        }
        ChartSource::Oci { reference } => args.push(reference.to_string()),
    }
    args.extend([
        "--version".to_string(),
        chart_version.to_string(),
        "--values".to_string(),
        values_path.display().to_string(),
        "--include-crds".to_string(),
        // Without --no-hooks the chart emits its hook Jobs (e.g. the
        // tigera-operator-uninstall Job), which this controller would then
        // apply as live objects -- immediately tearing the component back down.
        "--no-hooks".to_string(),
        // Without an explicit namespace, helm resolves .Release.Namespace from
        // ambient kubeconfig context, so namespaced objects land in the wrong
        // namespace (or "default") instead of the chart's own namespace.
        "--namespace".to_string(),
        chart.namespace.to_string(),
    ]);
    args
}

pub fn build_render_args(chart_version: &str, values_path: &std::path::Path) -> Vec<String> {
    render_args(&CALICO_CHART, chart_version, values_path)
}

pub async fn render_chart(
    chart: &ChartRef,
    chart_version: &str,
    values: &serde_json::Value,
) -> Result<String, HelmError> {
    let yaml = serde_yaml::to_string(values).expect("serde_json::Value always serializes to YAML");

    let mut file = tempfile::NamedTempFile::new().map_err(HelmError::WriteValues)?;
    {
        use std::io::Write;
        file.write_all(yaml.as_bytes()).map_err(HelmError::WriteValues)?;
    }

    let args = render_args(chart, chart_version, file.path());
    let output = tokio::process::Command::new("helm")
        .args(&args)
        .output()
        .await
        .map_err(HelmError::Spawn)?;

    if !output.status.success() {
        return Err(HelmError::NonZeroExit {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(strip_oci_pull_preamble(&String::from_utf8_lossy(&output.stdout)).to_string())
}

/// `helm template` on an `oci://` chart prints `Pulled:` and `Digest:` progress
/// lines to stdout ahead of the manifests. Left in place they parse as a
/// bogus first manifest with no `apiVersion`/`kind`, so drop them. Repo charts
/// print no such lines and pass through unchanged.
fn strip_oci_pull_preamble(output: &str) -> &str {
    let mut rest = output;
    while let Some(line_end) = rest.find('\n') {
        let line = &rest[..line_end];
        if line.starts_with("Pulled: ") || line.starts_with("Digest: ") {
            rest = &rest[line_end + 1..];
        } else {
            break;
        }
    }
    rest
}

pub async fn render(calico: &CalicoSpec) -> Result<String, HelmError> {
    render_chart(&CALICO_CHART, &calico.chart_version, &build_values(calico)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{CalicoIpPoolSpec, CalicoSpec, Encapsulation};

    fn sample_spec() -> CalicoSpec {
        CalicoSpec {
            chart_version: "v3.29.1".to_string(),
            bgp_enabled: true,
            api_server_enabled: true,
            ip_pools: vec![CalicoIpPoolSpec {
                name: "pods-v6".to_string(),
                cidr: "fd97:45c2:b3a1:1100::/56".to_string(),
                encapsulation: Encapsulation::None,
                nat_outgoing: true,
                block_size: Some(122),
                node_selector: "all()".to_string(),
                disabled: false,
            }],
            node_address_autodetection_v6_cidrs: vec!["fd97:45c2:b3a1:179::/64".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn disables_flex_volume_unconditionally() {
        let values = build_values(&sample_spec());

        assert_eq!(values["installation"]["flexVolumePath"], "None");
    }

    #[test]
    fn translates_bools_to_enabled_disabled_strings() {
        let values = build_values(&sample_spec());

        assert_eq!(values["installation"]["calicoNetwork"]["bgp"], "Enabled");
        assert_eq!(values["apiServer"]["enabled"], true);
        assert_eq!(
            values["installation"]["calicoNetwork"]["ipPools"][0]["natOutgoing"],
            "Enabled"
        );
    }

    #[test]
    fn places_node_address_autodetection_at_calico_network_level_not_per_pool() {
        let values = build_values(&sample_spec());

        assert_eq!(
            values["installation"]["calicoNetwork"]["nodeAddressAutodetectionV6"]["cidrs"][0],
            "fd97:45c2:b3a1:179::/64"
        );
        assert!(values["installation"]["calicoNetwork"]["ipPools"][0]
            .get("nodeAddressAutodetectionV6")
            .is_none());
    }

    #[test]
    fn maps_encapsulation_variants_to_chart_strings() {
        let mut spec = sample_spec();
        spec.ip_pools[0].encapsulation = Encapsulation::Vxlan;
        let values = build_values(&spec);
        assert_eq!(
            values["installation"]["calicoNetwork"]["ipPools"][0]["encapsulation"],
            "VXLAN"
        );
    }

    #[test]
    fn render_args_pin_chart_repo_and_version() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = build_render_args("v3.29.1", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "calico".to_string(),
                "--repo".to_string(),
                "https://docs.tigera.io/calico/charts".to_string(),
                "tigera-operator".to_string(),
                "--version".to_string(),
                "v3.29.1".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "tigera-operator".to_string(),
            ]
        );
    }

    #[test]
    fn oci_render_args_pass_the_reference_without_a_repo_flag() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&SPEGEL_CHART, "v0.0.0-test", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "spegel".to_string(),
                "oci://ghcr.io/spegel-org/helm-charts/spegel".to_string(),
                "--version".to_string(),
                "v0.0.0-test".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "spegel".to_string(),
            ]
        );
    }

    #[test]
    fn cert_manager_render_args_use_the_oci_form_and_the_cert_manager_namespace() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&CERT_MANAGER_CHART, "v1.16.2", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "cert-manager".to_string(),
                "oci://quay.io/jetstack/charts/cert-manager".to_string(),
                "--version".to_string(),
                "v1.16.2".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "cert-manager".to_string(),
            ]
        );
    }

    #[test]
    fn openstack_ccm_render_args_use_the_repo_form_and_kube_system() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&OPENSTACK_CCM_CHART, "2.36.5", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "openstack-ccm".to_string(),
                "--repo".to_string(),
                "https://kubernetes.github.io/cloud-provider-openstack".to_string(),
                "openstack-cloud-controller-manager".to_string(),
                "--version".to_string(),
                "2.36.5".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "kube-system".to_string(),
            ]
        );
    }

    #[test]
    fn openstack_cinder_csi_render_args_use_the_repo_form_and_kube_system() {
        let path = std::path::Path::new("/tmp/values.yaml");
        let args = render_args(&OPENSTACK_CINDER_CSI_CHART, "2.36.5", path);

        assert_eq!(
            args,
            vec![
                "template".to_string(),
                "cinder-csi".to_string(),
                "--repo".to_string(),
                "https://kubernetes.github.io/cloud-provider-openstack".to_string(),
                "openstack-cinder-csi".to_string(),
                "--version".to_string(),
                "2.36.5".to_string(),
                "--values".to_string(),
                "/tmp/values.yaml".to_string(),
                "--include-crds".to_string(),
                "--no-hooks".to_string(),
                "--namespace".to_string(),
                "kube-system".to_string(),
            ]
        );
    }

    #[test]
    fn strips_the_oci_pull_progress_lines_before_the_first_manifest() {
        let output = "Pulled: ghcr.io/spegel-org/helm-charts/spegel:0.7.4\n\
                      Digest: sha256:abc\n\
                      ---\n\
                      # Source: spegel/templates/rbac.yaml\n\
                      kind: ServiceAccount\n";

        assert_eq!(
            strip_oci_pull_preamble(output),
            "---\n# Source: spegel/templates/rbac.yaml\nkind: ServiceAccount\n"
        );
    }

    #[test]
    fn leaves_repo_chart_output_untouched() {
        let output = "---\n# Source: tigera-operator/templates/x.yaml\nkind: Deployment\n";

        assert_eq!(strip_oci_pull_preamble(output), output);
    }

    #[test]
    fn omits_node_address_autodetection_v6_when_no_cidrs_configured() {
        let mut spec = sample_spec();
        spec.node_address_autodetection_v6_cidrs = vec![];

        let values = build_values(&spec);

        assert!(values["installation"]["calicoNetwork"]
            .get("nodeAddressAutodetectionV6")
            .is_none());
    }

    #[test]
    fn kubernetes_internal_ip_renders_the_operators_kubernetes_method() {
        let mut spec = sample_spec();
        spec.node_address_autodetection_v6_cidrs = vec![];
        spec.node_address_autodetection_v6_method =
            crate::crd::NodeAddressAutodetectionMethod::KubernetesInternalIp;

        let values = build_values(&spec);

        assert_eq!(
            values["installation"]["calicoNetwork"]["nodeAddressAutodetectionV6"],
            serde_json::json!({ "kubernetes": "NodeInternalIP" })
        );
    }

    #[test]
    fn the_default_method_still_renders_the_cidrs() {
        let values = build_values(&sample_spec());

        assert_eq!(
            values["installation"]["calicoNetwork"]["nodeAddressAutodetectionV6"],
            serde_json::json!({ "cidrs": ["fd97:45c2:b3a1:179::/64"] })
        );
    }

    #[test]
    fn passes_pool_names_so_the_operator_adopts_the_explicit_ip_pool() {
        let values = build_values(&sample_spec());

        assert_eq!(
            values["installation"]["calicoNetwork"]["ipPools"][0]["name"],
            "pods-v6"
        );
    }

    #[test]
    fn resolves_omitted_block_size_by_address_family() {
        let mut spec = sample_spec();
        spec.ip_pools[0].block_size = None;

        let values = build_values(&spec);

        assert_eq!(
            values["installation"]["calicoNetwork"]["ipPools"][0]["blockSize"],
            122
        );
    }

    #[test]
    fn omits_disabled_pools_from_the_installation_but_keeps_enabled_siblings() {
        let mut spec = sample_spec();
        let retired = CalicoIpPoolSpec {
            name: "pods-retired".to_string(),
            cidr: "fd00:db8:0:2200::/56".to_string(),
            disabled: true,
            ..spec.ip_pools[0].clone()
        };
        spec.ip_pools.push(retired);

        let values = build_values(&spec);
        let pools = values["installation"]["calicoNetwork"]["ipPools"]
            .as_array()
            .expect("ipPools is an array");

        // The operator would recreate a disabled pool as enabled if it were
        // declared on the Installation.
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0]["name"], "pods-v6");
    }

    #[test]
    fn ipv6_only_values_enable_no_ipv4() {
        let values = build_values(&sample_spec());
        let network = &values["installation"]["calicoNetwork"];

        assert!(network.get("nodeAddressAutodetectionV4").is_none());
        for pool in network["ipPools"].as_array().expect("ipPools is an array") {
            assert!(pool["cidr"].as_str().unwrap().contains(':'), "pool must be IPv6: {pool}");
        }
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn spegel_chart_renders_parseable_manifests_with_our_values() {
        let spegel = crate::pull_through_cache::SpegelSpec {
            chart_version: "0.7.4".to_string(),
            registries: Some(vec![
                "https://docker.io".to_string(),
                "https://ghcr.io".to_string(),
            ]),
            ..Default::default()
        };
        let values = crate::pull_through_cache::build_values(&spegel);

        let rendered = render_chart(&SPEGEL_CHART, &spegel.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        // Every document must be a real Kubernetes object; the OCI `Pulled:` lines
        // would otherwise parse as one with no apiVersion/kind.
        for object in &objects {
            let types = object.types.as_ref().expect("every rendered object has a type");
            assert!(!types.kind.is_empty(), "{object:?}");
        }
        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().unwrap().kind.as_str())
            .collect();
        assert!(kinds.contains(&"DaemonSet"), "{kinds:?}");
        assert!(rendered.contains("--containerd-registry-config-path=/etc/cri/conf.d/hosts"));
        // Spegel requires URLs; the entries reach the DaemonSet args unchanged.
        assert!(rendered.contains("- \"https://docker.io\""), "{rendered}");
        // --no-hooks: the post-delete cleanup hook must not be rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn openstack_ccm_chart_renders_the_shape_the_spec_relies_on() {
        let openstack = crate::cloud_controller_manager::OpenstackSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: crate::cloud_controller_manager::SecretNameRef {
                name: "my-cloud-config".to_string(),
            },
            ..Default::default()
        };
        let values = crate::cloud_controller_manager::build_values(&openstack);

        let rendered = render_chart(&OPENSTACK_CCM_CHART, &openstack.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().expect("every rendered object has a type").kind.as_str())
            .collect();
        // The controller never creates the credentials Secret; the user does.
        assert!(!kinds.contains(&"Secret"), "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| **kind == "DaemonSet").count(), 1, "{kinds:?}");

        let daemon_set = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "DaemonSet")
            .expect("one DaemonSet");
        assert_eq!(daemon_set.metadata.namespace.as_deref(), Some("kube-system"));

        // Only the cloud-config Secret volume remains: the hostPath mounts are gone.
        let volumes = daemon_set
            .data
            .pointer("/spec/template/spec/volumes")
            .and_then(|value| value.as_array())
            .expect("the DaemonSet has volumes");
        assert_eq!(volumes.len(), 1, "{volumes:?}");
        assert_eq!(volumes[0]["secret"]["secretName"], "my-cloud-config");
        assert!(!rendered.contains("hostPath"), "{rendered}");

        // The chart defaults to dnsPolicy: ClusterFirstWithHostNet, which would
        // deadlock this exact bootstrap (CoreDNS cannot schedule until the CCM
        // clears every node's uninitialized taint, and the CCM cannot resolve its
        // cloud's endpoint without CoreDNS). The controller always overrides it.
        assert_eq!(
            daemon_set.data.pointer("/spec/template/spec/dnsPolicy"),
            Some(&serde_json::json!("Default"))
        );

        // The chart reads the config from the `cloud.conf` key of that Secret, and
        // its Role is scoped to the same name.
        assert!(rendered.contains("/etc/config/cloud.conf"), "{rendered}");
        assert!(rendered.contains("- my-cloud-config"), "{rendered}");

        // --no-hooks: no hook objects are rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn openstack_cinder_csi_chart_renders_the_shape_the_spec_relies_on() {
        let openstack_cinder = crate::csi_driver::OpenstackCinderSpec {
            chart_version: "2.36.5".to_string(),
            cloud_config_secret_ref: crate::crd::SecretNameRef {
                name: "my-cloud-config".to_string(),
            },
            storage_classes: crate::csi_driver::StorageClassesSpec {
                additional: vec![
                    crate::csi_driver::AdditionalStorageClassSpec {
                        name: "csi-cinder-sc-az1".to_string(),
                        reclaim_policy: crate::csi_driver::ReclaimPolicy::Delete,
                        parameters: std::collections::BTreeMap::from([(
                            "availability".to_string(),
                            "az1".to_string(),
                        )]),
                        is_default: false,
                    },
                    crate::csi_driver::AdditionalStorageClassSpec {
                        name: "csi-cinder-sc-az2".to_string(),
                        reclaim_policy: crate::csi_driver::ReclaimPolicy::Retain,
                        parameters: std::collections::BTreeMap::from([(
                            "availability".to_string(),
                            "az2".to_string(),
                        )]),
                        is_default: false,
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let values = crate::csi_driver::build_values(&openstack_cinder);

        let rendered = render_chart(&OPENSTACK_CINDER_CSI_CHART, &openstack_cinder.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().expect("every rendered object has a type").kind.as_str())
            .collect();

        // The controller never creates the credentials Secret; the user does.
        assert!(!kinds.contains(&"Secret"), "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| **kind == "DaemonSet").count(), 1, "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| **kind == "Deployment").count(), 1, "{kinds:?}");
        assert!(kinds.contains(&"CSIDriver"), "{kinds:?}");

        let csi_driver_object = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "CSIDriver")
            .expect("one CSIDriver");
        assert_eq!(csi_driver_object.metadata.name.as_deref(), Some("cinder.csi.openstack.org"));

        let node_plugin = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "DaemonSet")
            .expect("one DaemonSet");
        assert_eq!(node_plugin.metadata.namespace.as_deref(), Some("kube-system"));

        let volumes = node_plugin
            .data
            .pointer("/spec/template/spec/volumes")
            .and_then(|value| value.as_array())
            .expect("the DaemonSet has volumes");
        let secret_volume = volumes
            .iter()
            .find(|volume| volume.get("secret").is_some())
            .expect("a Secret volume");
        assert_eq!(secret_volume["secret"]["secretName"], "my-cloud-config");

        // The chart reads the config from the `cloud.conf` key of that Secret.
        assert!(rendered.contains("/etc/config/cloud.conf"), "{rendered}");

        // The chart's default csi.plugin.volumes hostPath-mounts /etc/cacert on
        // both plugin containers. Talos's root filesystem is read-only and never
        // creates that directory, so the container runtime's mkdir for the bind
        // mount fails outright (live-verified against a real Talos cluster: every
        // cinder-csi-plugin container sat in CreateContainerError). The override
        // must drop it from both the node and controller plugin.
        assert!(
            !volumes.iter().any(|volume| volume["name"] == "cacert"),
            "the node plugin must not mount /etc/cacert on Talos: {volumes:?}"
        );
        let controller_plugin_volumes = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "Deployment")
            .expect("one Deployment")
            .data
            .pointer("/spec/template/spec/volumes")
            .and_then(|value| value.as_array())
            .expect("the Deployment has volumes");
        assert!(
            !controller_plugin_volumes.iter().any(|volume| volume["name"] == "cacert"),
            "the controller plugin must not mount /etc/cacert on Talos: {controller_plugin_volumes:?}"
        );

        // Unlike the node plugin, the controller plugin has no hostNetwork and no
        // toleration: it needs a working CNI and the uninitialized taint cleared
        // before it can schedule and run. This is why the deploy README documents
        // CsiDriver as applying after both CloudControllerManager and CniInstallation.
        let controller_plugin = objects
            .iter()
            .find(|o| o.types.as_ref().unwrap().kind == "Deployment")
            .expect("one Deployment");
        assert!(
            controller_plugin.data.pointer("/spec/template/spec/hostNetwork").is_none(),
            "the controller plugin runs on the pod network"
        );
        let controller_tolerations = controller_plugin
            .data
            .pointer("/spec/template/spec/tolerations")
            .and_then(|value| value.as_array());
        assert!(
            controller_tolerations.is_none_or(|tolerations| tolerations.is_empty()),
            "{controller_tolerations:?}"
        );

        // storageClasses.default defaults to `delete`: only csi-cinder-sc-delete is
        // annotated as the cluster default.
        let delete_class = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-delete"))
            .expect("the delete-reclaim StorageClass");
        let retain_class = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-retain"))
            .expect("the retain-reclaim StorageClass");
        assert_eq!(
            delete_class
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("storageclass.kubernetes.io/is-default-class"))
                .map(String::as_str),
            Some("true")
        );
        assert!(
            retain_class
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("storageclass.kubernetes.io/is-default-class"))
                .is_none()
        );

        // storageClasses.additional renders each entry as a real extra
        // StorageClass through the chart's own storageClass.custom raw-YAML
        // extension point -- with two entries, confirming the `---\n`-joined
        // documents concatenate into valid multi-document YAML the chart
        // accepts (the multi-AZ scenario that motivated this feature).
        let extra_class_az1 = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-az1"))
            .expect("the az1 additional StorageClass");
        assert_eq!(extra_class_az1.types.as_ref().unwrap().kind, "StorageClass");
        assert_eq!(extra_class_az1.data["provisioner"], "cinder.csi.openstack.org");
        assert_eq!(extra_class_az1.data["reclaimPolicy"], "Delete");
        assert_eq!(extra_class_az1.data["parameters"]["availability"], "az1");

        let extra_class_az2 = objects
            .iter()
            .find(|o| o.metadata.name.as_deref() == Some("csi-cinder-sc-az2"))
            .expect("the az2 additional StorageClass");
        assert_eq!(extra_class_az2.types.as_ref().unwrap().kind, "StorageClass");
        assert_eq!(extra_class_az2.data["provisioner"], "cinder.csi.openstack.org");
        assert_eq!(extra_class_az2.data["reclaimPolicy"], "Retain");
        assert_eq!(extra_class_az2.data["parameters"]["availability"], "az2");

        // --no-hooks: no hook objects are rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn cert_manager_chart_renders_the_shape_the_spec_relies_on() {
        let spec = crate::cert_manager::CertManagerInstallationSpec {
            platform_kind: crate::crd::PlatformKind::TalosLinux,
            chart_version: "v1.16.2".to_string(),
            helm_values: None,
        };
        let values = crate::cert_manager::build_values(&spec);

        let rendered = render_chart(&CERT_MANAGER_CHART, &spec.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o.types.as_ref().expect("every rendered object has a type").kind.as_str())
            .collect();

        // crds.enabled: true is always set (build_values), so the chart's own six
        // CRDs are always included; this chart renders no Namespace and no Secret.
        let crd_names: Vec<&str> = objects
            .iter()
            .filter(|o| o.types.as_ref().unwrap().kind == "CustomResourceDefinition")
            .map(|o| o.metadata.name.as_deref().unwrap())
            .collect();
        assert_eq!(
            crd_names,
            vec![
                "certificaterequests.cert-manager.io",
                "certificates.cert-manager.io",
                "challenges.acme.cert-manager.io",
                "clusterissuers.cert-manager.io",
                "issuers.cert-manager.io",
                "orders.acme.cert-manager.io",
            ],
            "{crd_names:?}"
        );
        assert!(!kinds.contains(&"Namespace"), "{kinds:?}");
        assert!(!kinds.contains(&"Secret"), "{kinds:?}");

        // Exactly the controller, webhook and cainjector Deployments.
        let deployment_names: Vec<&str> = objects
            .iter()
            .filter(|o| o.types.as_ref().unwrap().kind == "Deployment")
            .map(|o| o.metadata.name.as_deref().unwrap())
            .collect();
        assert_eq!(
            {
                let mut sorted = deployment_names.clone();
                sorted.sort_unstable();
                sorted
            },
            vec!["cert-manager", "cert-manager-cainjector", "cert-manager-webhook"],
            "{deployment_names:?}"
        );
        assert!(kinds.contains(&"ValidatingWebhookConfiguration"), "{kinds:?}");
        assert!(kinds.contains(&"MutatingWebhookConfiguration"), "{kinds:?}");

        // No hostPath/hostNetwork anywhere: confirms the synthesized namespace
        // needs no pod-security.kubernetes.io/* labels, unlike Calico/Spegel.
        assert!(!rendered.contains("hostPath"), "{rendered}");
        assert!(!rendered.contains("hostNetwork"), "{rendered}");

        // --no-hooks: no hook objects are rendered as live objects.
        assert!(!rendered.contains("helm.sh/hook"));
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn cert_manager_omitting_crds_enabled_renders_no_crds_by_default() {
        // Pins the chart-default fact build_values relies on: without the typed
        // override, this chart ships no CRDs at all (unlike Calico, where CRDs
        // come from the chart in older versions and from the running operator in
        // newer ones).
        let rendered = render_chart(&CERT_MANAGER_CHART, "v1.16.2", &serde_json::json!({}))
            .await
            .expect("helm template should succeed");
        let objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        assert!(
            !objects.iter().any(|o| o.types.as_ref().unwrap().kind == "CustomResourceDefinition"),
            "chart default changed: CRDs now render without crds.enabled"
        );
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn spegel_0_7_4_keeps_exactly_one_mirror_target_after_the_ipv6_patch() {
        let spegel = crate::pull_through_cache::SpegelSpec {
            chart_version: "0.7.4".to_string(),
            ..Default::default()
        };
        let values = crate::pull_through_cache::build_values(&spegel);
        let rendered = render_chart(&SPEGEL_CHART, &spegel.chart_version, &values)
            .await
            .expect("helm template should succeed");
        let mut objects = crate::manifests::parse_manifests(&rendered).expect("manifests should parse");

        // Chart 0.7.4 emits a hostPort target and a NodePort target.
        let removed = crate::pull_through_cache::drop_unbracketed_node_ip_mirror_targets(&mut objects);

        assert_eq!(removed, 1);
        let daemon_set = objects
            .iter()
            .find(|o| o.types.as_ref().is_some_and(|t| t.kind == "DaemonSet"))
            .expect("the chart renders a DaemonSet");
        let args: Vec<&str> = daemon_set.data["spec"]["template"]["spec"]["initContainers"][0]["args"]
            .as_array()
            .expect("args is an array")
            .iter()
            .filter_map(|a| a.as_str())
            .collect();
        let targets: Vec<&&str> = args.iter().filter(|a| a.starts_with("http://$(NODE_IP):")).collect();
        assert_eq!(targets, vec![&"http://$(NODE_IP):30020"], "{args:?}");
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn v3_32_1_chart_ships_no_crds() {
        let spec = crate::crd::CalicoSpec {
            chart_version: "v3.32.1".to_string(),
            ..Default::default()
        };

        let rendered = render(&spec).await.expect("helm template should succeed");

        // From 3.32 the operator creates every CRD at runtime; the reconciler's
        // kind-availability wait depends on this.
        assert!(!rendered.contains("kind: CustomResourceDefinition"));
        assert!(rendered.contains("kind: Installation"));
    }

    #[tokio::test]
    #[ignore = "requires network access and the helm CLI to be installed"]
    async fn render_produces_deployment_manifest_for_tigera_operator() {
        let spec = crate::crd::CalicoSpec {
            chart_version: "v3.29.1".to_string(),
            ..Default::default()
        };

        let rendered = render(&spec).await.expect("helm template should succeed");

        assert!(rendered.contains("kind: Deployment"));
        assert!(rendered.contains("tigera-operator"));
    }
}
