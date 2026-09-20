use crate::cidr::{self, Family};
use crate::crd::{CalicoSpec, Encapsulation};
use std::collections::HashSet;
use std::net::IpAddr;

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum SpecError {
    #[error(transparent)]
    InvalidCidr(#[from] cidr::CidrError),
    #[error("invalid BGP peerIP {0:?}: expected an IP address")]
    InvalidPeerIp(String),
    #[error(
        "mixed IPv4 and IPv6 addresses in one installation are not supported \
         (dual-stack is a future feature)"
    )]
    MixedAddressFamilies,
    #[error(
        "ipPool {pool:?}: blockSize {block_size} is outside {min}-{max}, the valid range for \
         {family:?} pools"
    )]
    BlockSizeOutOfRange {
        pool: String,
        block_size: i32,
        family: Family,
        min: i32,
        max: i32,
    },
    #[error("spec.calico.bgp is set but bgpEnabled is false")]
    BgpRequiresBgpEnabled,
    #[error("duplicate {kind} name {name:?}")]
    DuplicateName { kind: &'static str, name: String },
    #[error("nodeAddressAutodetectionV6Cidrs entry {0:?} is not an IPv6 CIDR")]
    AutodetectionNotIpv6(String),
    #[error("ipPool {0:?}: IPIP encapsulation is not supported on IPv6 pools")]
    IpipOnIpv6(String),
}

impl SpecError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            SpecError::InvalidCidr(_)
            | SpecError::InvalidPeerIp(_)
            | SpecError::AutodetectionNotIpv6(_) => "InvalidAddress",
            SpecError::MixedAddressFamilies => "MixedAddressFamilies",
            SpecError::BlockSizeOutOfRange { .. } => "InvalidBlockSize",
            SpecError::BgpRequiresBgpEnabled => "InvalidBgpConfig",
            SpecError::DuplicateName { .. } => "DuplicateName",
            SpecError::IpipOnIpv6(_) => "InvalidEncapsulation",
        }
    }
}

/// Rejects specs Calico would reject or mis-handle, before anything is applied,
/// so a bad spec never leaves a half-configured cluster.
pub fn validate_calico(calico: &CalicoSpec) -> Result<(), SpecError> {
    if calico.bgp.is_some() && !calico.bgp_enabled {
        return Err(SpecError::BgpRequiresBgpEnabled);
    }

    let mut families: Vec<Family> = Vec::new();

    // Pod pools and LB pools both become `IPPool` objects, so their names share
    // one namespace.
    let mut pool_names: HashSet<&str> = HashSet::new();

    for pool in &calico.ip_pools {
        if !pool_names.insert(pool.name.as_str()) {
            return Err(SpecError::DuplicateName {
                kind: "IPPool",
                name: pool.name.clone(),
            });
        }

        let family = cidr::parse(&pool.cidr)?.family();

        if family == Family::V6 && pool.encapsulation == Encapsulation::Ipip {
            return Err(SpecError::IpipOnIpv6(pool.name.clone()));
        }

        let block_size = pool.effective_block_size();
        let range = family.block_size_range();
        if !range.contains(&block_size) {
            return Err(SpecError::BlockSizeOutOfRange {
                pool: pool.name.clone(),
                block_size,
                family,
                min: *range.start(),
                max: *range.end(),
            });
        }

        families.push(family);
    }

    for pool in &calico.load_balancer_pools {
        if !pool_names.insert(pool.name.as_str()) {
            return Err(SpecError::DuplicateName {
                kind: "IPPool",
                name: pool.name.clone(),
            });
        }
        families.push(cidr::parse(&pool.cidr)?.family());
    }

    if let Some(bgp) = &calico.bgp {
        for entry in &bgp.service_load_balancer_ips {
            families.push(cidr::parse(entry)?.family());
        }

        let mut peer_names: HashSet<&str> = HashSet::new();
        for peer in &bgp.peers {
            if !peer_names.insert(peer.name.as_str()) {
                return Err(SpecError::DuplicateName {
                    kind: "BGPPeer",
                    name: peer.name.clone(),
                });
            }

            let ip: IpAddr = peer
                .peer_ip
                .parse()
                .map_err(|_| SpecError::InvalidPeerIp(peer.peer_ip.clone()))?;
            families.push(if ip.is_ipv6() { Family::V6 } else { Family::V4 });
        }
    }

    for entry in &calico.node_address_autodetection_v6_cidrs {
        if cidr::parse(entry)?.family() != Family::V6 {
            return Err(SpecError::AutodetectionNotIpv6(entry.clone()));
        }
        families.push(Family::V6);
    }

    if families.contains(&Family::V4) && families.contains(&Family::V6) {
        return Err(SpecError::MixedAddressFamilies);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        BgpPeerSpec, BgpSpec, CalicoIpPoolSpec, CalicoSpec, Encapsulation, LoadBalancerPoolSpec,
        LogSeverity,
    };

    fn pool(name: &str, cidr: &str) -> CalicoIpPoolSpec {
        CalicoIpPoolSpec {
            name: name.to_string(),
            cidr: cidr.to_string(),
            encapsulation: Encapsulation::None,
            nat_outgoing: true,
            block_size: None,
            node_selector: "all()".to_string(),
        }
    }

    fn lb_pool(name: &str, cidr: &str) -> LoadBalancerPoolSpec {
        LoadBalancerPoolSpec {
            name: name.to_string(),
            cidr: cidr.to_string(),
            node_selector: "all()".to_string(),
            disabled: false,
        }
    }

    fn bgp() -> BgpSpec {
        BgpSpec {
            as_number: 64514,
            node_to_node_mesh_enabled: true,
            log_severity_screen: LogSeverity::Info,
            service_load_balancer_ips: vec!["fd00:db8:0:f00::/112".to_string()],
            peers: vec![BgpPeerSpec {
                name: "gateway".to_string(),
                peer_ip: "fd00:db8:0:179::1".to_string(),
                as_number: 64512,
            }],
        }
    }

    fn valid_ipv6() -> CalicoSpec {
        CalicoSpec {
            chart_version: "v3.32.1".to_string(),
            bgp_enabled: true,
            api_server_enabled: true,
            ip_pools: vec![pool("pods-v6", "fd00:db8:0:1100::/56")],
            node_address_autodetection_v6_cidrs: vec!["fd00:db8:0:179::/64".to_string()],
            bgp: Some(bgp()),
            load_balancer_pools: vec![lb_pool("lb-internal-routed", "fd00:db8:0:f00::/112")],
        }
    }

    #[test]
    fn accepts_a_full_ipv6_only_spec() {
        assert_eq!(validate_calico(&valid_ipv6()), Ok(()));
    }

    #[test]
    fn accepts_an_ipv4_only_spec() {
        let spec = CalicoSpec {
            chart_version: "v3.29.1".to_string(),
            ip_pools: vec![pool("default", "10.244.0.0/16")],
            ..Default::default()
        };

        assert_eq!(validate_calico(&spec), Ok(()));
    }

    #[test]
    fn accepts_an_empty_spec() {
        assert_eq!(validate_calico(&CalicoSpec::default()), Ok(()));
    }

    #[test]
    fn rejects_an_unparseable_pool_cidr() {
        let mut spec = valid_ipv6();
        spec.ip_pools[0].cidr = "fd00:db8:0:1100::".to_string();

        let err = validate_calico(&spec).expect_err("cidr without prefix must be rejected");

        assert!(matches!(err, SpecError::InvalidCidr(_)));
        assert_eq!(err.reason(), "InvalidAddress");
    }

    #[test]
    fn rejects_an_unparseable_service_load_balancer_cidr() {
        let mut spec = valid_ipv6();
        spec.bgp.as_mut().unwrap().service_load_balancer_ips = vec!["nope".to_string()];

        assert!(matches!(validate_calico(&spec), Err(SpecError::InvalidCidr(_))));
    }

    #[test]
    fn rejects_an_unparseable_peer_ip() {
        let mut spec = valid_ipv6();
        spec.bgp.as_mut().unwrap().peers[0].peer_ip = "gateway.example".to_string();

        assert!(matches!(validate_calico(&spec), Err(SpecError::InvalidPeerIp(_))));
    }

    #[test]
    fn rejects_mixed_address_families() {
        let mut spec = valid_ipv6();
        spec.load_balancer_pools = vec![lb_pool("lb-v4", "192.0.2.0/24")];

        let err = validate_calico(&spec).expect_err("mixed families must be rejected");

        assert_eq!(err, SpecError::MixedAddressFamilies);
        assert_eq!(err.reason(), "MixedAddressFamilies");
    }

    #[test]
    fn rejects_an_ipv4_peer_in_an_ipv6_installation() {
        let mut spec = valid_ipv6();
        spec.bgp.as_mut().unwrap().peers[0].peer_ip = "192.0.2.1".to_string();

        assert_eq!(validate_calico(&spec), Err(SpecError::MixedAddressFamilies));
    }

    #[test]
    fn rejects_an_ipv6_block_size_on_an_ipv4_pool_and_vice_versa() {
        let mut v6 = valid_ipv6();
        v6.ip_pools[0].block_size = Some(26);
        let mut v4 = CalicoSpec {
            ip_pools: vec![pool("default", "10.244.0.0/16")],
            ..Default::default()
        };
        v4.ip_pools[0].block_size = Some(122);

        for spec in [v6, v4] {
            let err = validate_calico(&spec).expect_err("out-of-range blockSize must be rejected");
            assert!(matches!(err, SpecError::BlockSizeOutOfRange { .. }));
            assert_eq!(err.reason(), "InvalidBlockSize");
        }
    }

    #[test]
    fn rejects_bgp_without_bgp_enabled() {
        let mut spec = valid_ipv6();
        spec.bgp_enabled = false;

        let err = validate_calico(&spec).expect_err("bgp requires bgpEnabled");

        assert_eq!(err, SpecError::BgpRequiresBgpEnabled);
        assert_eq!(err.reason(), "InvalidBgpConfig");
    }

    #[test]
    fn rejects_duplicate_pod_pool_names() {
        let mut spec = valid_ipv6();
        spec.ip_pools.push(pool("pods-v6", "fd00:db8:0:1200::/56"));

        assert!(matches!(
            validate_calico(&spec),
            Err(SpecError::DuplicateName { kind: "IPPool", .. })
        ));
    }

    #[test]
    fn rejects_a_load_balancer_pool_reusing_a_pod_pool_name() {
        let mut spec = valid_ipv6();
        spec.load_balancer_pools[0].name = "pods-v6".to_string();

        assert!(matches!(
            validate_calico(&spec),
            Err(SpecError::DuplicateName { kind: "IPPool", .. })
        ));
    }

    #[test]
    fn rejects_duplicate_peer_names() {
        let mut spec = valid_ipv6();
        let duplicate = spec.bgp.as_ref().unwrap().peers[0].clone();
        spec.bgp.as_mut().unwrap().peers.push(duplicate);

        assert!(matches!(
            validate_calico(&spec),
            Err(SpecError::DuplicateName { kind: "BGPPeer", .. })
        ));
    }

    #[test]
    fn rejects_ipip_on_an_ipv6_pool_but_allows_it_on_ipv4() {
        let mut v6 = valid_ipv6();
        v6.ip_pools[0].encapsulation = Encapsulation::Ipip;
        let v4 = CalicoSpec {
            ip_pools: vec![CalicoIpPoolSpec {
                encapsulation: Encapsulation::Ipip,
                ..pool("default", "10.244.0.0/16")
            }],
            ..Default::default()
        };

        let err = validate_calico(&v6).expect_err("IPIP over IPv6 must be rejected");
        assert!(matches!(err, SpecError::IpipOnIpv6(_)));
        assert_eq!(err.reason(), "InvalidEncapsulation");
        assert_eq!(validate_calico(&v4), Ok(()));
    }

    #[test]
    fn rejects_an_ipv4_autodetection_cidr() {
        let mut spec = valid_ipv6();
        spec.node_address_autodetection_v6_cidrs = vec!["10.0.0.0/24".to_string()];

        assert!(matches!(
            validate_calico(&spec),
            Err(SpecError::AutodetectionNotIpv6(_))
        ));
    }

    #[test]
    fn rejects_v6_autodetection_in_an_ipv4_installation() {
        let spec = CalicoSpec {
            ip_pools: vec![pool("default", "10.244.0.0/16")],
            node_address_autodetection_v6_cidrs: vec!["fd00:db8:0:179::/64".to_string()],
            ..Default::default()
        };

        assert_eq!(validate_calico(&spec), Err(SpecError::MixedAddressFamilies));
    }
}
