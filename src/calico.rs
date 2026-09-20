use crate::crd::{CalicoIpPoolSpec, CalicoSpec, Encapsulation, LoadBalancerPoolSpec};
use kube::api::DynamicObject;

pub const CALICO_API_VERSION: &str = "crd.projectcalico.org/v1";

fn object(kind: &str, name: &str, spec: serde_json::Value) -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": CALICO_API_VERSION,
        "kind": kind,
        "metadata": { "name": name },
        "spec": spec,
    }))
    .expect("static Calico object JSON deserializes into a DynamicObject")
}

/// One explicit `IPPool` per `spec.calico.ipPools[]`.
///
/// The operator skips creating pools from `Installation` once any `IPPool`
/// exists, so declaring the pod pools explicitly keeps IPAM independent of
/// operator timing. Fields deliberately mirror what the operator would render
/// for the same `Installation` pool (same name, `Workload`+`Tunnel` uses,
/// explicit modes), so the two writers never fight over a field.
pub fn pod_pool_objects(calico: &CalicoSpec) -> Vec<DynamicObject> {
    calico.ip_pools.iter().map(pod_pool_object).collect()
}

fn pod_pool_object(pool: &CalicoIpPoolSpec) -> DynamicObject {
    let (ipip_mode, vxlan_mode) = match pool.encapsulation {
        Encapsulation::None => ("Never", "Never"),
        Encapsulation::Ipip => ("Always", "Never"),
        Encapsulation::Vxlan => ("Never", "Always"),
    };

    object(
        "IPPool",
        &pool.name,
        serde_json::json!({
            "cidr": pool.cidr,
            "blockSize": pool.effective_block_size(),
            "natOutgoing": pool.nat_outgoing,
            "nodeSelector": pool.node_selector,
            "ipipMode": ipip_mode,
            "vxlanMode": vxlan_mode,
            "allowedUses": ["Workload", "Tunnel"],
        }),
    )
}

/// `BGPConfiguration` (named `default`) and one `BGPPeer` per peer when
/// `spec.calico.bgp` is set, followed by one LoadBalancer-only `IPPool` per
/// `spec.calico.loadBalancerPools[]`. Applied only after the pod pools exist.
pub fn routing_and_lb_objects(calico: &CalicoSpec) -> Vec<DynamicObject> {
    let mut objects = Vec::new();

    if let Some(bgp) = &calico.bgp {
        let advertised: Vec<serde_json::Value> = bgp
            .service_load_balancer_ips
            .iter()
            .map(|cidr| serde_json::json!({ "cidr": cidr }))
            .collect();

        objects.push(object(
            "BGPConfiguration",
            "default",
            serde_json::json!({
                "asNumber": bgp.as_number,
                "nodeToNodeMeshEnabled": bgp.node_to_node_mesh_enabled,
                "logSeverityScreen": bgp.log_severity_screen,
                "serviceLoadBalancerIPs": advertised,
            }),
        ));

        objects.extend(bgp.peers.iter().map(|peer| {
            object(
                "BGPPeer",
                &peer.name,
                serde_json::json!({ "peerIP": peer.peer_ip, "asNumber": peer.as_number }),
            )
        }));
    }

    objects.extend(calico.load_balancer_pools.iter().map(load_balancer_pool_object));
    objects
}

fn load_balancer_pool_object(pool: &LoadBalancerPoolSpec) -> DynamicObject {
    object(
        "IPPool",
        &pool.name,
        serde_json::json!({
            "cidr": pool.cidr,
            "allowedUses": ["LoadBalancer"],
            "nodeSelector": pool.node_selector,
            "disabled": pool.disabled,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        BgpPeerSpec, BgpSpec, CalicoIpPoolSpec, CalicoSpec, Encapsulation, LoadBalancerPoolSpec,
        LogSeverity,
    };

    fn spec() -> CalicoSpec {
        CalicoSpec {
            chart_version: "v3.32.1".to_string(),
            bgp_enabled: true,
            ip_pools: vec![CalicoIpPoolSpec {
                name: "pods-v6".to_string(),
                cidr: "fd00:db8:0:1100::/56".to_string(),
                encapsulation: Encapsulation::None,
                nat_outgoing: true,
                block_size: None,
                node_selector: "all()".to_string(),
            }],
            bgp: Some(BgpSpec {
                as_number: 64514,
                node_to_node_mesh_enabled: true,
                log_severity_screen: LogSeverity::Info,
                service_load_balancer_ips: vec![
                    "fd00:db8:0:f00::/112".to_string(),
                    "2001:db8:0:27f::/112".to_string(),
                ],
                peers: vec![BgpPeerSpec {
                    name: "gateway".to_string(),
                    peer_ip: "fd00:db8:0:179::1".to_string(),
                    as_number: 64512,
                }],
            }),
            load_balancer_pools: vec![
                LoadBalancerPoolSpec {
                    name: "lb-internal-routed".to_string(),
                    cidr: "fd00:db8:0:f00::/112".to_string(),
                    node_selector: "all()".to_string(),
                    disabled: false,
                },
                LoadBalancerPoolSpec {
                    name: "lb-retired".to_string(),
                    cidr: "fd00:db8:0:ffff::/112".to_string(),
                    node_selector: "all()".to_string(),
                    disabled: true,
                },
            ],
            ..Default::default()
        }
    }

    fn kind(object: &kube::api::DynamicObject) -> &str {
        &object.types.as_ref().expect("types are set").kind
    }

    fn name(object: &kube::api::DynamicObject) -> &str {
        object.metadata.name.as_deref().expect("name is set")
    }

    #[test]
    fn pod_pool_mirrors_what_the_operator_would_render() {
        let objects = pod_pool_objects(&spec());

        assert_eq!(objects.len(), 1);
        let pool = &objects[0];
        assert_eq!(pool.types.as_ref().unwrap().api_version, "crd.projectcalico.org/v1");
        assert_eq!(kind(pool), "IPPool");
        assert_eq!(name(pool), "pods-v6");
        assert_eq!(
            pool.data["spec"],
            serde_json::json!({
                "cidr": "fd00:db8:0:1100::/56",
                "blockSize": 122,
                "natOutgoing": true,
                "nodeSelector": "all()",
                "ipipMode": "Never",
                "vxlanMode": "Never",
                "allowedUses": ["Workload", "Tunnel"],
            })
        );
    }

    #[test]
    fn pod_pool_encapsulation_maps_to_ipip_and_vxlan_modes() {
        let mut ipip = spec();
        ipip.ip_pools[0].cidr = "10.244.0.0/16".to_string();
        ipip.ip_pools[0].encapsulation = Encapsulation::Ipip;
        let mut vxlan = ipip.clone();
        vxlan.ip_pools[0].encapsulation = Encapsulation::Vxlan;

        let ipip_pool = &pod_pool_objects(&ipip)[0];
        let vxlan_pool = &pod_pool_objects(&vxlan)[0];

        assert_eq!(ipip_pool.data["spec"]["ipipMode"], "Always");
        assert_eq!(ipip_pool.data["spec"]["vxlanMode"], "Never");
        assert_eq!(ipip_pool.data["spec"]["blockSize"], 26);
        assert_eq!(vxlan_pool.data["spec"]["ipipMode"], "Never");
        assert_eq!(vxlan_pool.data["spec"]["vxlanMode"], "Always");
    }

    #[test]
    fn explicit_block_size_wins_over_the_family_default() {
        let mut explicit = spec();
        explicit.ip_pools[0].block_size = Some(124);

        assert_eq!(pod_pool_objects(&explicit)[0].data["spec"]["blockSize"], 124);
    }

    #[test]
    fn no_pod_pools_yields_no_objects() {
        assert!(pod_pool_objects(&CalicoSpec::default()).is_empty());
    }

    #[test]
    fn routing_objects_are_bgp_configuration_then_peers_then_lb_pools() {
        let objects = routing_and_lb_objects(&spec());

        let kinds_and_names: Vec<(&str, &str)> =
            objects.iter().map(|o| (kind(o), name(o))).collect();
        assert_eq!(
            kinds_and_names,
            vec![
                ("BGPConfiguration", "default"),
                ("BGPPeer", "gateway"),
                ("IPPool", "lb-internal-routed"),
                ("IPPool", "lb-retired"),
            ]
        );
    }

    #[test]
    fn bgp_configuration_carries_as_mesh_logging_and_advertised_vips() {
        let objects = routing_and_lb_objects(&spec());

        assert_eq!(
            objects[0].data["spec"],
            serde_json::json!({
                "asNumber": 64514,
                "nodeToNodeMeshEnabled": true,
                "logSeverityScreen": "Info",
                "serviceLoadBalancerIPs": [
                    { "cidr": "fd00:db8:0:f00::/112" },
                    { "cidr": "2001:db8:0:27f::/112" },
                ],
            })
        );
    }

    #[test]
    fn bgp_peer_carries_peer_ip_and_as() {
        let objects = routing_and_lb_objects(&spec());

        assert_eq!(
            objects[1].data["spec"],
            serde_json::json!({ "peerIP": "fd00:db8:0:179::1", "asNumber": 64512 })
        );
    }

    #[test]
    fn load_balancer_pools_are_loadbalancer_only_and_honour_disabled() {
        let objects = routing_and_lb_objects(&spec());

        assert_eq!(
            objects[2].data["spec"],
            serde_json::json!({
                "cidr": "fd00:db8:0:f00::/112",
                "allowedUses": ["LoadBalancer"],
                "nodeSelector": "all()",
                "disabled": false,
            })
        );
        assert_eq!(objects[3].data["spec"]["disabled"], true);
    }

    #[test]
    fn without_bgp_only_load_balancer_pools_are_rendered() {
        let mut no_bgp = spec();
        no_bgp.bgp = None;

        let objects = routing_and_lb_objects(&no_bgp);

        assert!(objects.iter().all(|o| kind(o) == "IPPool"));
        assert_eq!(objects.len(), 2);
    }
}
