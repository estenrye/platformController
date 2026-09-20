use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    /// Calico's valid IPAM block sizes for a pool of this family.
    pub fn block_size_range(self) -> std::ops::RangeInclusive<i32> {
        match self {
            Family::V4 => 20..=32,
            Family::V6 => 116..=128,
        }
    }

    /// Calico's own default block size for this family.
    pub fn default_block_size(self) -> i32 {
        match self {
            Family::V4 => 26,
            Family::V6 => 122,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn family(&self) -> Family {
        if self.addr.is_ipv6() {
            Family::V6
        } else {
            Family::V4
        }
    }
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
#[error("invalid CIDR {0:?}: expected <address>/<prefix-length>")]
pub struct CidrError(pub String);

pub fn parse(input: &str) -> Result<Cidr, CidrError> {
    let err = || CidrError(input.to_string());

    let (addr, prefix) = input.split_once('/').ok_or_else(err)?;
    let addr: IpAddr = addr.parse().map_err(|_| err())?;
    let prefix: u8 = prefix.parse().map_err(|_| err())?;

    let max = if addr.is_ipv6() { 128 } else { 32 };
    if prefix > max {
        return Err(err());
    }

    Ok(Cidr { addr, prefix })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_ipv6_cidr() {
        let cidr = parse("fd00:db8:0:1100::/56").expect("valid IPv6 CIDR");

        assert_eq!(cidr.prefix, 56);
        assert_eq!(cidr.family(), Family::V6);
    }

    #[test]
    fn parses_an_ipv4_cidr() {
        let cidr = parse("10.244.0.0/16").expect("valid IPv4 CIDR");

        assert_eq!(cidr.prefix, 16);
        assert_eq!(cidr.family(), Family::V4);
    }

    #[test]
    fn rejects_input_without_a_prefix_length() {
        assert_eq!(parse("10.244.0.0"), Err(CidrError("10.244.0.0".to_string())));
    }

    #[test]
    fn rejects_a_non_numeric_prefix_length() {
        assert!(parse("10.244.0.0/abc").is_err());
    }

    #[test]
    fn rejects_a_prefix_length_beyond_the_family_maximum() {
        assert!(parse("10.244.0.0/33").is_err());
        assert!(parse("fd00::/129").is_err());
        assert!(parse("fd00::/128").is_ok());
    }

    #[test]
    fn rejects_a_malformed_address() {
        assert!(parse("not-an-ip/64").is_err());
    }

    #[test]
    fn block_size_ranges_match_calico() {
        assert_eq!(Family::V4.block_size_range(), 20..=32);
        assert_eq!(Family::V6.block_size_range(), 116..=128);
    }

    #[test]
    fn default_block_sizes_are_inside_their_own_range() {
        assert_eq!(Family::V4.default_block_size(), 26);
        assert_eq!(Family::V6.default_block_size(), 122);
        assert!(Family::V4.block_size_range().contains(&Family::V4.default_block_size()));
        assert!(Family::V6.block_size_range().contains(&Family::V6.default_block_size()));
    }
}
