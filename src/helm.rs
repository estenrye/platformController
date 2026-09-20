use crate::crd::{CalicoSpec, Encapsulation};

/// Namespace the tigera-operator chart's namespaced objects belong in. The chart
/// itself renders no `Namespace` object, so the controller both renders into and
/// creates this namespace explicitly.
pub const TIGERA_OPERATOR_NAMESPACE: &str = "tigera-operator";

pub fn build_values(calico: &CalicoSpec) -> serde_json::Value {
    let ip_pools: Vec<serde_json::Value> = calico
        .ip_pools
        .iter()
        .map(|pool| {
            serde_json::json!({
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
    // when no CIDRs are configured.
    if !calico.node_address_autodetection_v6_cidrs.is_empty() {
        calico_network["nodeAddressAutodetectionV6"] = serde_json::json!({
            "cidrs": calico.node_address_autodetection_v6_cidrs,
        });
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

pub fn build_render_args(chart_version: &str, values_path: &std::path::Path) -> Vec<String> {
    vec![
        "template".to_string(),
        "calico".to_string(),
        "--repo".to_string(),
        "https://projectcalico.docs.tigera.io/charts".to_string(),
        "tigera-operator".to_string(),
        "--version".to_string(),
        chart_version.to_string(),
        "--values".to_string(),
        values_path.display().to_string(),
        "--include-crds".to_string(),
        // Without --no-hooks the chart emits its pre-delete uninstall Job
        // (tigera-operator-uninstall), which this controller would then apply as
        // a live object -- immediately tearing Calico back down.
        "--no-hooks".to_string(),
        // Without an explicit namespace, helm resolves .Release.Namespace from
        // ambient kubeconfig context, so namespaced objects land in the wrong
        // namespace (or "default") instead of tigera-operator.
        "--namespace".to_string(),
        TIGERA_OPERATOR_NAMESPACE.to_string(),
    ]
}

pub async fn render(calico: &CalicoSpec) -> Result<String, HelmError> {
    let values = build_values(calico);
    let yaml = serde_yaml::to_string(&values).expect("serde_json::Value always serializes to YAML");

    let mut file = tempfile::NamedTempFile::new().map_err(HelmError::WriteValues)?;
    {
        use std::io::Write;
        file.write_all(yaml.as_bytes()).map_err(HelmError::WriteValues)?;
    }

    let args = build_render_args(&calico.chart_version, file.path());
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

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
                "https://projectcalico.docs.tigera.io/charts".to_string(),
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
    fn omits_node_address_autodetection_v6_when_no_cidrs_configured() {
        let mut spec = sample_spec();
        spec.node_address_autodetection_v6_cidrs = vec![];

        let values = build_values(&spec);

        assert!(values["installation"]["calicoNetwork"]
            .get("nodeAddressAutodetectionV6")
            .is_none());
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
