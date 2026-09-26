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
