use platform_controller::crd::CniInstallation;
use platform_controller::{calico, helm, reconciler};

const EXAMPLE: &str = "examples/cni-installation-ipv6.yaml";

fn names(objects: &[kube::api::DynamicObject]) -> Vec<(String, String)> {
    objects
        .iter()
        .map(|o| {
            (
                o.types.as_ref().unwrap().kind.clone(),
                o.metadata.name.clone().unwrap(),
            )
        })
        .collect()
}

fn load() -> CniInstallation {
    let content = std::fs::read_to_string(EXAMPLE).expect("the IPv6 example should exist");
    serde_yaml::from_str(&content).expect("the IPv6 example should deserialize as a CniInstallation")
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_targets_chart_v3_32_1_with_bgp_and_the_api_server() {
    let calico = load().spec.calico;

    assert_eq!(calico.chart_version, "v3.32.1");
    assert!(calico.bgp_enabled);
    assert!(calico.api_server_enabled);
}

#[test]
fn example_enables_no_ipv4_anywhere() {
    let installation = load();
    let values = helm::build_values(&installation.spec.calico);
    let network = &values["installation"]["calicoNetwork"];

    assert!(network.get("nodeAddressAutodetectionV4").is_none());
    for pool in network["ipPools"].as_array().unwrap() {
        assert!(pool["cidr"].as_str().unwrap().contains(':'));
    }
    assert_eq!(network["ipPools"][0]["blockSize"], 122);
    assert_eq!(network["ipPools"][0]["name"], "pods-v6");
}

#[test]
fn example_renders_pod_pool_then_bgp_then_load_balancer_objects() {
    let calico_spec = load().spec.calico;

    let pods = calico::pod_pool_objects(&calico_spec);
    let routing = calico::routing_and_lb_objects(&calico_spec);

    assert_eq!(names(&pods), vec![("IPPool".to_string(), "pods-v6".to_string())]);
    assert_eq!(
        names(&routing),
        vec![
            ("BGPConfiguration".to_string(), "default".to_string()),
            ("BGPPeer".to_string(), "gateway".to_string()),
            ("IPPool".to_string(), "lb-internal-routed".to_string()),
            ("IPPool".to_string(), "lb-ingress-routed".to_string()),
        ]
    );
}

#[test]
fn every_advertised_vip_range_is_backed_by_a_load_balancer_pool() {
    let calico = load().spec.calico;
    let bgp = calico.bgp.expect("the example configures BGP");

    for advertised in &bgp.service_load_balancer_ips {
        assert!(
            calico.load_balancer_pools.iter().any(|pool| &pool.cidr == advertised),
            "{advertised} is advertised over BGP but no loadBalancerPools entry owns it"
        );
    }
}

#[test]
fn example_uses_placeholder_addressing_that_cannot_collide_with_the_flux_cluster() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the IPv6 example should exist");
    let bgp = load().spec.calico.bgp.expect("the example configures BGP");

    assert_ne!(bgp.as_number, 64513, "64513 belongs to the existing active cluster");
    assert_eq!(bgp.as_number, 64514);
    assert!(!content.contains("fd97:45c2:b3a1"), "real ULA prefix must not be committed");
    assert!(!content.contains("2607:3640"), "real GUA prefix must not be committed");
}
