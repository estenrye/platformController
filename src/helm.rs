use crate::crd::{CalicoSpec, Encapsulation};

pub fn build_values(calico: &CalicoSpec) -> serde_json::Value {
    let ip_pools: Vec<serde_json::Value> = calico
        .ip_pools
        .iter()
        .map(|pool| {
            serde_json::json!({
                "cidr": pool.cidr,
                "encapsulation": encapsulation_str(&pool.encapsulation),
                "natOutgoing": bool_to_enum(pool.nat_outgoing),
                "blockSize": pool.block_size,
                "nodeSelector": pool.node_selector,
            })
        })
        .collect();

    serde_json::json!({
        "installation": {
            "enabled": true,
            "calicoNetwork": {
                "bgp": bool_to_enum(calico.bgp_enabled),
                "ipPools": ip_pools,
                "nodeAddressAutodetectionV6": {
                    "cidrs": calico.node_address_autodetection_v6_cidrs,
                },
            },
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
                block_size: 122,
                node_selector: "all()".to_string(),
            }],
            node_address_autodetection_v6_cidrs: vec!["fd97:45c2:b3a1:179::/64".to_string()],
        }
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
}
