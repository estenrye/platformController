# IPv6-Only Calico with Full Flux Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `CniInstallation` express everything the Flux `applications/calico/controlplane` app configures for an IPv6-only Talos cluster (explicit pod pools, BGP configuration and peers, LoadBalancer pools), on Calico v3.32.1, with an IPv6-only example and a live verification runbook.

**Architecture:** Add typed `bgp` and `loadBalancerPools` fields to `spec.calico`; a pure `calico` module turns the spec into `crd.projectcalico.org/v1` objects; `reconcile` applies them in explicit phases after a new generic "kind is registered" discovery wait (chart v3.32.1 ships no CRDs, so the operator creates them at runtime). A pure `cidr` module and `spec_validation` module reject bad specs before any apply.

**Tech Stack:** Rust 2024, kube-rs 4.2, k8s-openapi, schemars, serde/serde_yaml/serde_json, thiserror, tokio. No new dependencies.

**Spec:** [docs/superpowers/specs/2026-09-19-ipv6-only-calico-design.md](../specs/2026-09-19-ipv6-only-calico-design.md)

## Global Constraints

- Chart repository: `https://docs.tigera.io/calico/charts`. Chart `v3.32.1` for the new example; native LoadBalancer IPAM requires Calico 3.30+.
- Address family is inferred from CIDRs, never a flag. Mixed IPv4/IPv6 in one installation is rejected (dual-stack is a future spec).
- Valid `blockSize`: 20-32 for IPv4 pools, 116-128 for IPv6 pools. Omitted `blockSize` resolves to 26 (IPv4) or 122 (IPv6).
- `spec.calico.bgp` requires `bgpEnabled: true`. `encapsulation: IPIP` is rejected on IPv6 pools.
- Pool names are unique across `ipPools` and `loadBalancerPools` together (both become `IPPool` object names). Peer names are unique.
- Calico objects use `apiVersion: crd.projectcalico.org/v1`. Pod pools: `allowedUses: [Workload, Tunnel]`. LB pools: `allowedUses: [LoadBalancer]`.
- Pod pools are also passed to the operator's `Installation` values **with the same `name`**, so the operator adopts the one object instead of creating a duplicate.
- Apply order: Namespace, chart objects (rank order), pod pools, then `BGPConfiguration`/`BGPPeer`/LB pools. Before applying any custom resource or Calico object, wait for its kind to be registered via API discovery (deadline 180s).
- Every networked call in a polling loop is wrapped in `tokio::time::timeout_at`, not just the loop (see `docs/memory/wait-for-crd-established.md`).
- Example addressing is placeholders only: cluster AS `64514`, `fd00:db8:0:*` ULA-style ranges, `2001:db8:0:27f::/112`. Real addressing is never committed. The example must not reuse AS `64513` or any `fd97:45c2:b3a1` / `2607:3640` prefix.
- Commit messages end with the trailer `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>` (pass it as a second `-m`).
- Memory files live in `docs/memory/` (see CLAUDE.md), are indexed in `docs/memory/MEMORY.md`, and are committed like any change.

## File Structure

| File | Responsibility |
|---|---|
| `src/cidr.rs` (new) | Pure CIDR parsing, address family, per-family block size range and default |
| `src/crd.rs` (modify) | New spec types (`BgpSpec`, `BgpPeerSpec`, `LoadBalancerPoolSpec`, `LogSeverity`), optional `blockSize` |
| `src/spec_validation.rs` (new) | `validate_calico`: all pre-apply spec rejections, each with a condition reason |
| `src/calico.rs` (new) | Pure builders: spec to `IPPool` / `BGPConfiguration` / `BGPPeer` `DynamicObject`s |
| `src/helm.rs` (modify) | Chart repo constant, pool `name` and resolved `blockSize` in values |
| `src/apply.rs` (modify) | `is_missing_kind_error`, `wait_for_kind_available`, `KindNotAvailable` |
| `src/manifests.rs` (modify) | `is_custom_resource` |
| `src/reconciler.rs` (modify) | Wire validation, kind waits and phased apply |
| `examples/cni-installation-ipv6.yaml` (new) | IPv6-only example |
| `tests/ipv6_example.rs` (new) | Example-driven end-to-end (offline) test |
| `tests/bootstrap_manifests.rs` (modify) | CRD drift test |
| `deploy/crd.yaml` (regenerate) | CRD manifest |
| `docs/runbooks/ipv6-only-kvm-verification.md` (new) | Live acceptance checklist |
| `docs/memory/*` (modify/new) | RBAC ledger, project memory, index |

---

## Task 1: CIDR parsing and address families

**Files:**
- Create: `src/cidr.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 2, 3):
  - `pub enum Family { V4, V6 }` (`Debug, Clone, Copy, PartialEq, Eq`) with `pub fn block_size_range(self) -> std::ops::RangeInclusive<i32>` and `pub fn default_block_size(self) -> i32`
  - `pub struct Cidr { pub addr: std::net::IpAddr, pub prefix: u8 }` with `pub fn family(&self) -> Family`
  - `pub struct CidrError(pub String)` (`thiserror`, `Debug, PartialEq, Eq`)
  - `pub fn parse(input: &str) -> Result<Cidr, CidrError>`

- [ ] **Step 1: Write the failing tests**

Create `src/cidr.rs` containing only the test module (the items are added in Step 3):

```rust
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
```

Add `pub mod cidr;` to `src/lib.rs` so the file is:

```rust
pub mod apply;
pub mod cidr;
pub mod crd;
pub mod helm;
pub mod leader;
pub mod manifests;
pub mod reconciler;
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib cidr::`
Expected: FAIL to compile (`cannot find function parse`, `cannot find type Family`).

- [ ] **Step 3: Implement**

Prepend to `src/cidr.rs` (above the test module):

```rust
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
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib cidr::`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/cidr.rs src/lib.rs
git commit -m "feat: add CIDR parsing with address-family block size rules" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 2: Extend the CRD types

**Files:**
- Modify: `src/crd.rs` (`CalicoSpec`, `CalicoIpPoolSpec`, defaults, tests)
- Modify: `src/helm.rs` (test literals only; `build_values` changes in Task 4)
- Modify: `src/reconciler.rs:342-348` (test literal)
- Modify: `tests/bootstrap_manifests.rs` (drift test)
- Regenerate: `deploy/crd.yaml`

**Interfaces:**
- Consumes: `cidr::parse`, `cidr::Family` (Task 1).
- Produces (used by Tasks 3-8):
  - `CalicoSpec` gains `#[derive(Default)]`, `pub bgp: Option<BgpSpec>`, `pub load_balancer_pools: Vec<LoadBalancerPoolSpec>`
  - `CalicoIpPoolSpec.block_size: Option<i32>` and `pub fn effective_block_size(&self) -> i32`
  - `pub struct BgpSpec { pub as_number: u32, pub node_to_node_mesh_enabled: bool, pub log_severity_screen: LogSeverity, pub service_load_balancer_ips: Vec<String>, pub peers: Vec<BgpPeerSpec> }`
  - `pub struct BgpPeerSpec { pub name: String, pub peer_ip: String, pub as_number: u32 }`
  - `pub struct LoadBalancerPoolSpec { pub name: String, pub cidr: String, pub node_selector: String, pub disabled: bool }`
  - `pub enum LogSeverity { Debug, Info, Warning, Error, Fatal }` (default `Info`)

- [ ] **Step 1: Write the failing tests**

In `src/crd.rs` tests module, change the existing assertions and add tests.

Change in `spec_round_trips_through_json`:
```rust
        assert_eq!(spec.calico.ip_pools[0].block_size, Some(122));
```

Replace `ip_pool_defaults_apply_when_omitted` with:
```rust
    #[test]
    fn ip_pool_defaults_apply_when_omitted() {
        let json = serde_json::json!({
            "name": "default",
            "cidr": "10.244.0.0/16"
        });

        let pool: CalicoIpPoolSpec = serde_json::from_value(json).expect("pool should deserialize");

        assert_eq!(pool.encapsulation, Encapsulation::None);
        assert!(pool.nat_outgoing);
        assert_eq!(pool.block_size, None);
        assert_eq!(pool.effective_block_size(), 26);
        assert_eq!(pool.node_selector, "all()");
    }

    #[test]
    fn effective_block_size_defaults_by_address_family() {
        let v6: CalicoIpPoolSpec = serde_json::from_value(serde_json::json!({
            "name": "pods-v6",
            "cidr": "fd00:db8:0:1100::/56"
        }))
        .expect("pool should deserialize");
        let explicit: CalicoIpPoolSpec = serde_json::from_value(serde_json::json!({
            "name": "pods-v6",
            "cidr": "fd00:db8:0:1100::/56",
            "blockSize": 124
        }))
        .expect("pool should deserialize");

        assert_eq!(v6.effective_block_size(), 122);
        assert_eq!(explicit.effective_block_size(), 124);
    }

    #[test]
    fn bgp_spec_defaults_apply_when_omitted() {
        let bgp: BgpSpec = serde_json::from_value(serde_json::json!({ "asNumber": 64514 }))
            .expect("bgp should deserialize");

        assert_eq!(bgp.as_number, 64514);
        assert!(bgp.node_to_node_mesh_enabled);
        assert_eq!(bgp.log_severity_screen, LogSeverity::Info);
        assert!(bgp.service_load_balancer_ips.is_empty());
        assert!(bgp.peers.is_empty());
    }

    #[test]
    fn load_balancer_pool_defaults_apply_when_omitted() {
        let pool: LoadBalancerPoolSpec = serde_json::from_value(serde_json::json!({
            "name": "lb-internal-routed",
            "cidr": "fd00:db8:0:f00::/112"
        }))
        .expect("pool should deserialize");

        assert_eq!(pool.node_selector, "all()");
        assert!(!pool.disabled);
    }

    #[test]
    fn calico_spec_without_bgp_or_load_balancer_pools_still_deserializes() {
        let spec: CalicoSpec = serde_json::from_value(serde_json::json!({ "chartVersion": "v3.29.1" }))
            .expect("existing specs must keep working");

        assert!(spec.bgp.is_none());
        assert!(spec.load_balancer_pools.is_empty());
    }

    #[test]
    fn full_ipv6_spec_with_bgp_and_load_balancer_pools_deserializes() {
        let spec: CalicoSpec = serde_json::from_value(serde_json::json!({
            "chartVersion": "v3.32.1",
            "bgpEnabled": true,
            "bgp": {
                "asNumber": 64514,
                "logSeverityScreen": "Warning",
                "serviceLoadBalancerIPs": ["fd00:db8:0:f00::/112"],
                "peers": [{ "name": "gateway", "peerIP": "fd00:db8:0:179::1", "asNumber": 64512 }]
            },
            "loadBalancerPools": [
                { "name": "lb-internal-routed", "cidr": "fd00:db8:0:f00::/112", "disabled": true }
            ]
        }))
        .expect("spec should deserialize");

        let bgp = spec.bgp.expect("bgp should be present");
        assert_eq!(bgp.log_severity_screen, LogSeverity::Warning);
        assert_eq!(bgp.peers[0].peer_ip, "fd00:db8:0:179::1");
        assert_eq!(bgp.peers[0].as_number, 64512);
        assert_eq!(spec.load_balancer_pools[0].name, "lb-internal-routed");
        assert!(spec.load_balancer_pools[0].disabled);
    }
```

Append to `tests/bootstrap_manifests.rs`:
```rust
#[test]
fn crd_yaml_matches_the_generated_crd() {
    use kube::CustomResourceExt;

    let generated = serde_yaml::to_string(&platform_controller::crd::CniInstallation::crd())
        .expect("CRD should serialize to YAML");
    let on_disk = std::fs::read_to_string("deploy/crd.yaml").expect("deploy/crd.yaml should exist");

    assert_eq!(
        on_disk, generated,
        "deploy/crd.yaml is stale; run `cargo run -q --bin crdgen > deploy/crd.yaml`"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib crd:: 2>&1 | tail -20`
Expected: FAIL to compile (`cannot find type BgpSpec`, `no field bgp`, `effective_block_size`).

- [ ] **Step 3: Implement the types**

In `src/crd.rs`, change `CalicoSpec` to:

```rust
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CalicoSpec {
    pub chart_version: String,
    #[serde(default)]
    pub bgp_enabled: bool,
    #[serde(default)]
    pub api_server_enabled: bool,
    #[serde(default)]
    pub ip_pools: Vec<CalicoIpPoolSpec>,
    #[serde(default)]
    pub node_address_autodetection_v6_cidrs: Vec<String>,
    /// BGP configuration and peers. Requires `bgpEnabled: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgp: Option<BgpSpec>,
    /// LoadBalancer-only IP pools (native Calico LoadBalancer IPAM, Calico >= 3.30).
    #[serde(default)]
    pub load_balancer_pools: Vec<LoadBalancerPoolSpec>,
}
```

Change the `block_size` field in `CalicoIpPoolSpec` and delete the `default_block_size` function (and its doc comment). Add the impl:

```rust
    /// Omitted means "use Calico's default for this pool's address family",
    /// resolved at render time (26 for IPv4, 122 for IPv6) because a single
    /// fixed serde default cannot be right for both families.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_size: Option<i32>,
```

```rust
impl CalicoIpPoolSpec {
    /// The block size to render: the explicit value, else the default for the
    /// CIDR's family. An unparseable CIDR falls back to the IPv4 default;
    /// `spec_validation` rejects such a spec before anything renders.
    pub fn effective_block_size(&self) -> i32 {
        self.block_size.unwrap_or_else(|| {
            crate::cidr::parse(&self.cidr)
                .map(|cidr| cidr.family())
                .unwrap_or(crate::cidr::Family::V4)
                .default_block_size()
        })
    }
}
```

Add after `CalicoIpPoolSpec`'s impl (before `default_nat_outgoing`):

```rust
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BgpSpec {
    pub as_number: u32,
    #[serde(default = "default_true")]
    pub node_to_node_mesh_enabled: bool,
    #[serde(default)]
    pub log_severity_screen: LogSeverity,
    /// CIDRs of LoadBalancer VIP pools to advertise to BGP peers.
    #[serde(default, rename = "serviceLoadBalancerIPs")]
    pub service_load_balancer_ips: Vec<String>,
    #[serde(default)]
    pub peers: Vec<BgpPeerSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BgpPeerSpec {
    pub name: String,
    #[serde(rename = "peerIP")]
    pub peer_ip: String,
    pub as_number: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancerPoolSpec {
    pub name: String,
    pub cidr: String,
    #[serde(default = "default_node_selector")]
    pub node_selector: String,
    /// Retired pools stay declared with `disabled: true`: Calico's pool CIDR is
    /// immutable, so renumbering is a new-pool swap, not an in-place edit.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq, Eq)]
pub enum LogSeverity {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
    Fatal,
}

fn default_true() -> bool {
    true
}
```

- [ ] **Step 4: Fix the existing test literals so the crate compiles**

In `src/reconciler.rs`, replace the `CalicoSpec` literal in `spec_with` with:
```rust
            calico: CalicoSpec {
                chart_version: "v3.29.1".to_string(),
                ..Default::default()
            },
```

In `src/helm.rs` tests, change `sample_spec` to:
```rust
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
```
and in `render_produces_deployment_manifest_for_tigera_operator` replace the literal with:
```rust
        let spec = crate::crd::CalicoSpec {
            chart_version: "v3.29.1".to_string(),
            ..Default::default()
        };
```
(`build_values` still reads `pool.block_size` as an `i32` until Task 4; it will not compile yet. Do the one-line fix now so this task compiles: in `build_values` change `"blockSize": pool.block_size,` to `"blockSize": pool.effective_block_size(),`.)

- [ ] **Step 5: Regenerate the CRD and run everything**

Run: `cargo run -q --bin crdgen > deploy/crd.yaml && cargo test`
Expected: PASS, including `crd_yaml_matches_the_generated_crd`. (`git diff --stat deploy/crd.yaml` should show additions for `bgp`, `loadBalancerPools` and `blockSize` no longer defaulted.)

- [ ] **Step 6: Commit**

```bash
git add src/crd.rs src/helm.rs src/reconciler.rs tests/bootstrap_manifests.rs deploy/crd.yaml
git commit -m "feat: add bgp and loadBalancerPools to CalicoSpec, make blockSize family-defaulted" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 3: Spec validation

**Files:**
- Create: `src/spec_validation.rs`
- Modify: `src/lib.rs`, `src/reconciler.rs` (`ValidationError`, `validate`, the validation branch of `reconcile`)

**Interfaces:**
- Consumes: `CalicoSpec` and friends (Task 2), `cidr::{parse, Family, CidrError}` (Task 1).
- Produces:
  - `pub enum SpecError` with `pub fn reason(&self) -> &'static str`
  - `pub fn validate_calico(calico: &CalicoSpec) -> Result<(), SpecError>`
  - `ValidationError::Spec(SpecError)` and `pub fn reason(&self) -> &'static str` on `ValidationError` (returns `"Unsupported"` for the pre-existing variants)

- [ ] **Step 1: Write the failing tests**

Create `src/spec_validation.rs` containing only:

```rust
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
```

Add `pub mod spec_validation;` to `src/lib.rs` (after `reconciler`).

In `src/reconciler.rs` tests module add:
```rust
    #[test]
    fn validate_surfaces_spec_errors_with_their_own_reason() {
        let mut spec = spec_with(PlatformKind::TalosLinux, CniProvider::Calico);
        spec.calico.bgp = Some(crate::crd::BgpSpec {
            as_number: 64514,
            node_to_node_mesh_enabled: true,
            log_severity_screen: crate::crd::LogSeverity::Info,
            service_load_balancer_ips: vec![],
            peers: vec![],
        });

        let err = validate("default", &spec).expect_err("bgp without bgpEnabled is invalid");

        assert_eq!(err.reason(), "InvalidBgpConfig");
    }

    #[test]
    fn pre_existing_validation_errors_keep_the_unsupported_reason() {
        let spec = spec_with(PlatformKind::TalosLinux, CniProvider::Calico);

        let err = validate("second", &spec).expect_err("non-singleton name is rejected");

        assert_eq!(err.reason(), "Unsupported");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib spec_validation:: reconciler::tests::validate_ 2>&1 | tail -20`
Expected: FAIL to compile (`cannot find function validate_calico`, `no method reason`).

- [ ] **Step 3: Implement `src/spec_validation.rs`**

Prepend above the test module:

```rust
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
```

- [ ] **Step 4: Wire it into `reconciler.rs`**

Add to the `ValidationError` enum:
```rust
    #[error(transparent)]
    Spec(#[from] crate::spec_validation::SpecError),
```
Add below the enum:
```rust
impl ValidationError {
    /// The `status.conditions[].reason` reported for this rejection.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::Spec(err) => err.reason(),
            _ => "Unsupported",
        }
    }
}
```
In `validate`, replace the final `Ok(())` with:
```rust
    crate::spec_validation::validate_calico(&spec.calico)?;
    Ok(())
```
In `reconcile`'s validation-failure branch, change the reason argument from `"Unsupported",` to `err.reason(),`.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/spec_validation.rs src/lib.rs src/reconciler.rs
git commit -m "feat: validate address families, block sizes, BGP config and names before apply" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 4: Chart repository, pool names and resolved block sizes in Helm values

**Files:**
- Modify: `src/helm.rs`

**Interfaces:**
- Consumes: `CalicoIpPoolSpec::effective_block_size` (Task 2).
- Produces: `pub const CALICO_CHART_REPO: &str`; `build_values` now emits `name` and the resolved `blockSize` per pool.

- [ ] **Step 1: Write the failing tests**

In `src/helm.rs` tests, replace the pinned repo string in `render_args_pin_chart_repo_and_version` (`"https://projectcalico.docs.tigera.io/charts"`) with `"https://docs.tigera.io/calico/charts"`, and add:

```rust
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
    fn ipv6_only_values_enable_no_ipv4() {
        let values = build_values(&sample_spec());
        let network = &values["installation"]["calicoNetwork"];

        assert!(network.get("nodeAddressAutodetectionV4").is_none());
        for pool in network["ipPools"].as_array().expect("ipPools is an array") {
            assert!(pool["cidr"].as_str().unwrap().contains(':'), "pool must be IPv6: {pool}");
        }
    }
```

Add an ignored network test next to the existing one:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib helm::tests 2>&1 | tail -20`
Expected: FAIL on `render_args_pin_chart_repo_and_version` (old URL) and `passes_pool_names...` (`Null`).

- [ ] **Step 3: Implement**

At the top of `src/helm.rs`, below `TIGERA_OPERATOR_NAMESPACE`:
```rust
/// The Helm repository the tigera-operator chart is fetched from.
pub const CALICO_CHART_REPO: &str = "https://docs.tigera.io/calico/charts";
```
In `build_render_args`, replace the `"https://projectcalico.docs.tigera.io/charts".to_string(),` element with `CALICO_CHART_REPO.to_string(),`.

In `build_values`, replace the per-pool `json!` with:
```rust
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
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib helm::tests`
Expected: PASS (network tests stay ignored). Then optionally, with network: `cargo test --lib helm::tests::v3_32_1 -- --ignored` should PASS.

- [ ] **Step 5: Commit**

```bash
git add src/helm.rs
git commit -m "feat: use the current Calico chart repo, pass pool names and family-resolved block sizes" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 5: Calico object builders

**Files:**
- Create: `src/calico.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `CalicoSpec`, `BgpSpec`, `LoadBalancerPoolSpec`, `Encapsulation` (Task 2).
- Produces (used by Tasks 7, 8):
  - `pub const CALICO_API_VERSION: &str = "crd.projectcalico.org/v1"`
  - `pub fn pod_pool_objects(calico: &CalicoSpec) -> Vec<kube::api::DynamicObject>` (one `IPPool` per `ipPools[]`)
  - `pub fn routing_and_lb_objects(calico: &CalicoSpec) -> Vec<kube::api::DynamicObject>` (`BGPConfiguration` named `default` if `bgp` is set, then one `BGPPeer` per peer, then one LB `IPPool` per `loadBalancerPools[]`)

- [ ] **Step 1: Write the failing tests**

Create `src/calico.rs` containing only:

```rust
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
```

Add `pub mod calico;` to `src/lib.rs` (after `apply`, before `cidr`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib calico::`
Expected: FAIL to compile (`cannot find function pod_pool_objects`).

- [ ] **Step 3: Implement**

Prepend to `src/calico.rs`:

```rust
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
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib calico::`
Expected: PASS (9 tests).

- [ ] **Step 5: Commit**

```bash
git add src/calico.rs src/lib.rs
git commit -m "feat: render IPPool, BGPConfiguration and BGPPeer objects from the spec" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 6: Kind-availability wait

**Files:**
- Modify: `src/apply.rs` (`ApplyError`, `dynamic_api_for`, new functions, tests)

**Interfaces:**
- Consumes: existing `group_version_kind`.
- Produces (used by Task 7):
  - `pub fn is_missing_kind_error(err: &kube::Error) -> bool`
  - `pub const KIND_AVAILABLE_TIMEOUT: Duration` (180s)
  - `pub async fn wait_for_kind_available(client: &kube::Client, api_version: &str, kind: &str, timeout: Duration) -> Result<(), ApplyError>`
  - `ApplyError::KindNotAvailable { api_version, kind, timeout, detail }`

- [ ] **Step 1: Write the failing tests**

In `src/apply.rs` tests module add:

```rust
    #[test]
    fn a_404_from_discovery_means_the_kind_is_not_registered() {
        let err = kube::Error::Api(Box::new(kube::core::Status {
            code: 404,
            ..Default::default()
        }));

        assert!(is_missing_kind_error(&err));
    }

    #[test]
    fn missing_kind_and_missing_group_discovery_errors_mean_not_registered() {
        for err in [
            kube::error::DiscoveryError::MissingKind("Installation".to_string()),
            kube::error::DiscoveryError::MissingApiGroup("operator.tigera.io".to_string()),
            kube::error::DiscoveryError::EmptyApiGroup("operator.tigera.io/v1".to_string()),
        ] {
            assert!(is_missing_kind_error(&kube::Error::Discovery(err)));
        }
    }

    #[test]
    fn other_api_errors_are_not_treated_as_missing_kind() {
        let err = kube::Error::Api(Box::new(kube::core::Status {
            code: 500,
            ..Default::default()
        }));

        assert!(!is_missing_kind_error(&err));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib apply::tests::a_404 2>&1 | tail -10`
Expected: FAIL to compile (`cannot find function is_missing_kind_error`). If `kube::core::Status` or the `Default` construction does not compile against kube 4.2, build the error the same way `dynamic_api_for` matches it (`kube::Error::Api(Box<Status>)`) and adjust only the test construction.

- [ ] **Step 3: Implement**

Add to `ApplyError`:
```rust
    #[error(
        "{api_version}/{kind} did not become available within {timeout:?} (last observed: {detail})"
    )]
    KindNotAvailable {
        api_version: String,
        kind: String,
        timeout: Duration,
        detail: String,
    },
```

Above `dynamic_api_for` add:

```rust
/// How long to wait for a custom resource's kind to be registered before
/// giving up. A first install on a CNI-less cluster includes the operator's
/// image pull and CRD registration, so this is deliberately generous.
pub const KIND_AVAILABLE_TIMEOUT: Duration = Duration::from_secs(180);

/// True when API discovery failed because the kind's CRD (and so its whole API
/// group/resource) is not registered, as opposed to a malformed reference or a
/// transient network failure.
pub fn is_missing_kind_error(err: &kube::Error) -> bool {
    match err {
        kube::Error::Api(status) => status.code == 404,
        kube::Error::Discovery(
            kube::error::DiscoveryError::MissingKind(_)
            | kube::error::DiscoveryError::MissingApiGroup(_)
            | kube::error::DiscoveryError::EmptyApiGroup(_),
        ) => true,
        _ => false,
    }
}

/// Polls API discovery until `api_version`/`kind` resolves. From Calico 3.32
/// the operator (not the chart) creates every CRD at startup, so the
/// controller cannot apply a CRD and wait on it; it can only wait for the kind
/// to appear. Every discovery call is wrapped in `timeout_at`: a wedged
/// connection would otherwise defeat the deadline (see
/// docs/memory/wait-for-crd-established.md).
pub async fn wait_for_kind_available(
    client: &kube::Client,
    api_version: &str,
    kind: &str,
    timeout: Duration,
) -> Result<(), ApplyError> {
    let types = kube::api::TypeMeta {
        api_version: api_version.to_string(),
        kind: kind.to_string(),
    };
    let gvk = group_version_kind(&types);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut detail = "no attempt completed".to_string();

    loop {
        match tokio::time::timeout_at(deadline, kube::discovery::oneshot::pinned_kind(client, &gvk)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(err)) => {
                detail = if is_missing_kind_error(&err) {
                    "kind not registered yet".to_string()
                } else {
                    format!("discovery error: {err}")
                };
                tracing::debug!(api_version, kind, %detail, "waiting for kind to be registered");
            }
            Err(_) => {
                return Err(ApplyError::KindNotAvailable {
                    api_version: api_version.to_string(),
                    kind: kind.to_string(),
                    timeout,
                    detail,
                });
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ApplyError::KindNotAvailable {
                api_version: api_version.to_string(),
                kind: kind.to_string(),
                timeout,
                detail,
            });
        }

        let next_poll = tokio::time::Instant::now() + Duration::from_secs(2);
        tokio::time::sleep_until(next_poll.min(deadline)).await;
    }
}
```

In `dynamic_api_for`, replace the three `Err(...)` arms that return `Ok(None)` (the `Err(kube::Error::Api(err)) if err.code == 404` arm and the `Err(kube::Error::Discovery(...))` arm, keeping their explanatory comment) with a single arm placed before the final `Err(source)`:

```rust
        // The kind's CRD (and therefore its whole API group/resource) has
        // already been removed from the cluster. During cleanup this means
        // the resource we were about to act on is unambiguously already
        // gone, not a real failure — without this, any retry after a CRD is
        // deleted would permanently wedge on rediscovering it. Any other
        // discovery error (a malformed reference, transient network
        // failure, etc.) still propagates.
        Err(err) if is_missing_kind_error(&err) => Ok(None),
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib apply::`
Expected: PASS (existing tests unchanged plus 3 new).

- [ ] **Step 5: Commit**

```bash
git add src/apply.rs
git commit -m "feat: wait for a custom resource's kind to be registered before applying it" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 7: Phased apply in `reconcile`

**Files:**
- Modify: `src/manifests.rs` (add `is_custom_resource` + test)
- Modify: `src/reconciler.rs` (`reconcile` apply section, helper, cleanup regression test)

**Interfaces:**
- Consumes: `calico::{pod_pool_objects, routing_and_lb_objects}` (Task 5), `apply::{wait_for_kind_available, KIND_AVAILABLE_TIMEOUT}` (Task 6), `manifests::apply_rank`, `CUSTOM_RESOURCE_RANK`.
- Produces: `pub fn is_custom_resource(obj: &DynamicObject) -> bool` in `manifests`.

- [ ] **Step 1: Write the failing tests**

In `src/manifests.rs` tests module add:

```rust
    #[test]
    fn is_custom_resource_is_true_only_for_kinds_outside_the_builtin_ranks() {
        let objects = parse_manifests(SAMPLE_MANIFESTS).expect("manifests should parse");

        let flags: Vec<bool> = objects.iter().map(is_custom_resource).collect();

        // Namespace, Deployment, CustomResourceDefinition, Installation
        assert_eq!(flags, vec![false, false, false, true]);
    }
```

In `src/reconciler.rs` tests module add:

```rust
    #[test]
    fn calico_objects_are_cleaned_up_as_custom_resources_before_the_operator() {
        let pool = applied_resource("IPPool", "pods-v6");
        let bgp_configuration = applied_resource("BGPConfiguration", "default");
        let peer = applied_resource("BGPPeer", "gateway");
        let deployment = applied_resource("Deployment", "tigera-operator");

        let (custom_resources, infra) = partition_for_cleanup(&[
            deployment.clone(),
            pool.clone(),
            bgp_configuration.clone(),
            peer.clone(),
        ]);

        assert_eq!(custom_resources, vec![pool, bgp_configuration, peer]);
        assert_eq!(infra, vec![deployment]);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib is_custom_resource 2>&1 | tail -10`
Expected: FAIL to compile (`cannot find function is_custom_resource`). (The cleanup test should already pass; it pins existing behavior the new objects rely on.)

- [ ] **Step 3: Implement `is_custom_resource`**

In `src/manifests.rs` after `apply_rank`:

```rust
/// True for objects whose kind is defined by a CRD rather than built into the
/// API server, i.e. objects that can only be applied once that CRD is
/// registered. From Calico 3.32 the operator, not the chart, registers them.
pub fn is_custom_resource(obj: &DynamicObject) -> bool {
    apply_rank(obj) == CUSTOM_RESOURCE_RANK
}
```

- [ ] **Step 4: Rewrite the apply section of `reconcile`**

In `src/reconciler.rs`, add this helper above `reconcile`:

```rust
async fn wait_for_object_kind(
    client: &Client,
    object: &DynamicObject,
) -> Result<(), crate::apply::ApplyError> {
    let types = object.types.clone().unwrap_or_default();
    crate::apply::wait_for_kind_available(
        client,
        &types.api_version,
        &types.kind,
        crate::apply::KIND_AVAILABLE_TIMEOUT,
    )
    .await
}
```

Replace the `for object in &objects { ... }` loop and the `tracing::info!(applied_count ...)` line that follows it with:

```rust
    // Phase 1: the chart's objects in rank order. Its custom resources
    // (Installation, APIServer, ...) come last, and their CRDs may not exist
    // yet: chart v3.29.x ships them, but v3.32.x leaves it to the running
    // operator, so wait for each kind to be registered first.
    for object in &objects {
        if crate::manifests::is_custom_resource(object) {
            wait_for_object_kind(&ctx.client, object).await?;
        }
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        if reference.kind == "CustomResourceDefinition" {
            crate::apply::wait_for_crd_established(
                &ctx.client,
                &reference.name,
                std::time::Duration::from_secs(10),
            )
            .await?;
            tracing::debug!(crd = %reference.name, "CRD established");
        }
        applied.push(reference);
    }

    // Phases 2 and 3: Calico's own objects. Pod pools go first so IPAM never
    // depends on operator timing; BGP configuration, peers and LoadBalancer
    // pools only after they exist.
    let calico_phases = [
        crate::calico::pod_pool_objects(&obj.spec.calico),
        crate::calico::routing_and_lb_objects(&obj.spec.calico),
    ];
    for phase in &calico_phases {
        for object in phase {
            wait_for_object_kind(&ctx.client, object).await?;
            let reference =
                crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
            applied.push(reference);
        }
    }
    tracing::info!(applied_count = applied.len(), "applied all objects");
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test && cargo clippy --all-targets`
Expected: PASS, no clippy errors.

- [ ] **Step 6: Commit**

```bash
git add src/manifests.rs src/reconciler.rs
git commit -m "feat: apply Calico pools, BGP config and peers in phases behind a kind wait" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 8: IPv6-only example and example-driven test

**Files:**
- Create: `examples/cni-installation-ipv6.yaml`
- Create: `tests/ipv6_example.rs`

**Interfaces:**
- Consumes: everything above (`crd::CniInstallation`, `reconciler::validate`, `helm::build_values`, `calico::*`).
- Produces: the example manifest, the offline regression test.

- [ ] **Step 1: Write the failing test**

Create `tests/ipv6_example.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test ipv6_example 2>&1 | tail -10`
Expected: FAIL (`the IPv6 example should exist`).

- [ ] **Step 3: Create the example**

Create `examples/cni-installation-ipv6.yaml`:

```yaml
# IPv6-only Calico on Talos, with BGP peering and LoadBalancer VIP pools.
#
# This is the CniInstallation equivalent of a hand-written tigera-operator
# values file plus BGPConfiguration, BGPPeer and IPPool manifests. Every
# address and the AS number below are PLACEHOLDERS: substitute your own before
# applying, and make sure none of them overlap a cluster that is already
# running against the same gateway.
#
#   placeholder              meaning
#   AS 64514                 this cluster's BGP AS (must differ from any other
#                            cluster peering with the same gateway)
#   fd00:db8:0:1100::/56     pod pool (ULA, SNAT'd to the node address on egress)
#   fd00:db8:0:179::/64      dedicated BGP peering segment; ::1 is the gateway
#   fd00:db8:0:f00::/112     internal LoadBalancer VIP pool (routed, not on-link)
#   2001:db8:0:27f::/112     ingress LoadBalancer VIP pool (documentation range)
#
# The gateway must accept this cluster's AS and peering /64 as a BGP neighbor
# before nodes will establish sessions (see
# docs/runbooks/ipv6-only-kvm-verification.md, step 0).
#
# No IPv4 pool and no IPv4 autodetection means the operator never enables IPv4.
apiVersion: platform.rye.ninja/v1alpha1
kind: CniInstallation
metadata:
  name: default
spec:
  platformKind: talos-linux
  provider: calico
  calico:
    # Native LoadBalancer IPAM needs Calico 3.30 or later.
    chartVersion: v3.32.1
    bgpEnabled: true
    apiServerEnabled: true

    # Rendered as an explicit IPPool (allowedUses Workload+Tunnel) and also
    # passed to the operator's Installation under the same name.
    ipPools:
      - name: pods-v6
        cidr: fd00:db8:0:1100::/56
        encapsulation: None # same L2 segment; the node mesh routes pods
        natOutgoing: true
        blockSize: 122 # optional: omitted resolves to 122 for IPv6

    # Pick the node address on the BGP peering segment. Unambiguous because that
    # /64 carries nothing else (no SLAAC GUA, no floating VIP).
    nodeAddressAutodetectionV6Cidrs:
      - fd00:db8:0:179::/64

    # BGPConfiguration (named "default") plus one BGPPeer per entry.
    bgp:
      asNumber: 64514
      nodeToNodeMeshEnabled: true
      logSeverityScreen: Info
      # Every range advertised here needs a matching loadBalancerPools entry.
      serviceLoadBalancerIPs:
        - fd00:db8:0:f00::/112
        - 2001:db8:0:27f::/112
      peers:
        - name: gateway
          peerIP: fd00:db8:0:179::1
          asNumber: 64512

    # LoadBalancer-only IPPools. Retired pools stay declared with
    # disabled: true, because Calico's pool CIDR is immutable.
    loadBalancerPools:
      - name: lb-internal-routed
        cidr: fd00:db8:0:f00::/112
      - name: lb-ingress-routed
        cidr: 2001:db8:0:27f::/112
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --test ipv6_example`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add examples/cni-installation-ipv6.yaml tests/ipv6_example.rs
git commit -m "feat: add IPv6-only CniInstallation example with example-driven tests" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Task 9: Runbook, memory and final verification

**Files:**
- Create: `docs/runbooks/ipv6-only-kvm-verification.md`
- Create: `docs/memory/ipv6-only-calico-2026-09.md`
- Modify: `docs/memory/rbac-cluster-admin-tradeoff.md`, `docs/memory/MEMORY.md`

**Interfaces:** none (documentation).

- [ ] **Step 1: Create the runbook**

Create `docs/runbooks/ipv6-only-kvm-verification.md`:

````markdown
# Runbook: verify the IPv6-only Calico install on a KVM Talos cluster

Acceptance gate for [the IPv6-only Calico design](../superpowers/specs/2026-09-19-ipv6-only-calico-design.md).
CI cannot simulate BGP peering with the gateway, so this checklist is run by
hand against a real Talos cluster (installed with CNI `none`, IPv6 only) after
each change to the Calico install path.

You need: `kubectl` (cluster-admin), `talosctl`, shell access to the gateway
(FRR `vtysh`), and an edited copy of
[examples/cni-installation-ipv6.yaml](../../examples/cni-installation-ipv6.yaml)
with your real, non-colliding AS number and prefixes.

## 0. Gateway prerequisite (outside this repo)

The gateway's FRR config must accept the new cluster as a BGP neighbor:

- the cluster AS you chose is allowed as a remote AS for dynamic neighbors;
- the cluster's BGP peering `/64` is inside the dynamic-neighbor range;
- inbound filters accept the new pod and VIP prefixes.

If another cluster already peers with this gateway, confirm the new AS number
and every new prefix are distinct from the existing cluster's.

## 1. Install

```bash
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f my-cni-installation-ipv6.yaml
kubectl get cni default -w
```

Expected: `status.phase` reaches `Ready`. On chart v3.32.1 the first reconcile
may spend up to a few minutes in "waiting for kind to be registered" while the
operator pulls its image and registers its CRDs; that is normal. Watch with:

```bash
kubectl -n platform-system logs deploy/platform-controller -f
```

A `KindNotAvailable` failure after 180s means the operator never registered
the CRDs: check `kubectl -n tigera-operator logs deploy/tigera-operator`.

## 2. Open verification items

These two behaviors were unverifiable offline. Record the outcome in the PR.

```bash
# (a) Exactly one pool per declared name; no operator-created duplicate.
kubectl get ippools.crd.projectcalico.org
# Expected: pods-v6, lb-internal-routed, lb-ingress-routed (and nothing else).

# (b) No unknown-field warning for flexVolumePath on the v3.32.1 Installation CRD.
kubectl -n platform-system logs deploy/platform-controller | grep -i "unknown field"
# Expected: no output. If it warns about flexVolumePath, stop and drop that
# key for chart versions that no longer define it.
```

## 3. IPv6-only nodes

```bash
kubectl get nodes -o wide
kubectl get installation default -o yaml | grep -iE "ipv4|nodeAddressAutodetectionV4"
```

Expected: all nodes `Ready`; `INTERNAL-IP` values are IPv6 only; the second
command prints nothing (the operator never enabled IPv4).

## 4. BGP sessions and advertised VIPs

On the gateway:

```bash
vtysh -c 'show bgp ipv6 summary'
```

Expected: one Established session per node from the peering `/64`, remote AS
equal to your cluster AS.

Create a test LoadBalancer service and confirm it receives a VIP from your pool
and that the gateway learns it:

```bash
kubectl create deployment vip-test --image=nginx --port=80
kubectl expose deployment vip-test --type=LoadBalancer --port=80
kubectl get svc vip-test        # EXTERNAL-IP is inside lb-internal-routed
vtysh -c 'show bgp ipv6 unicast <EXTERNAL-IP>/128'   # on the gateway
curl -g "http://[<EXTERNAL-IP>]/"                    # from a LAN client
```

Expected: the VIP is allocated from your LB pool, present in the gateway's BGP
table, and reachable. Clean up: `kubectl delete svc,deploy vip-test`.

## 5. Pod egress is SNAT'd to the node address

```bash
kubectl run egress --rm -it --image=nicolaka/netshoot --restart=Never -- \
  curl -6 -s https://ipv6.icanhazip.com
```

Expected: the address printed is the node's, not a pod-pool address (pods are
ULA; `natOutgoing: true` rewrites them on egress).

## 6. Delete and cleanup

```bash
kubectl delete cni default
kubectl get ippools.crd.projectcalico.org 2>&1
kubectl get ns tigera-operator 2>&1
```

Expected: the delete completes (finalizer removed), Calico objects (BGP, LB
pools, pod pools) are removed before the operator, and `tigera-operator` is
gone. On chart v3.32.x the operator-created CRDs remain after cleanup (same as
`helm uninstall`); that is expected.

## Record

Note the chart version, Talos version, and the result of each step in the PR
description.
````

- [ ] **Step 2: Update the RBAC ledger**

In `docs/memory/rbac-cluster-admin-tradeoff.md`, in the permission ledger table, replace the row beginning `| \`crd.projectcalico.org\`, \`projectcalico.org\`` with:

```markdown
| `crd.projectcalico.org` | `ippools`, `bgpconfigurations`, `bgppeers` (create/patch/delete, get/list via discovery) | Controller itself (added 2026-09-19, [[ipv6-only-calico-2026-09]]) | The controller now applies these directly: explicit pod `IPPool`s, LoadBalancer `IPPool`s, `BGPConfiguration`, `BGPPeer`. Also runs API discovery (`GET /apis/crd.projectcalico.org/v1`) to wait for the kind to be registered before applying. A scope-down that grants only create/patch will fail discovery silently as a timeout, not a 403. |
| `crd.projectcalico.org`, `projectcalico.org` | incl. `tier.networkpolicies`, `tiers` | `tigera-operator` chart | Found missing during final review |
```

- [ ] **Step 3: Add the project memory and index entry**

Create `docs/memory/ipv6-only-calico-2026-09.md`:

```markdown
---
name: ipv6-only-calico-2026-09
description: IPv6-only Calico slice (2026-09-19) - typed bgp/loadBalancerPools in CniInstallation, and the non-obvious Calico v3.32.1 behaviors it had to design around
metadata:
  type: project
---

`CniInstallation` gained `spec.calico.bgp` (BGPConfiguration + peers) and `spec.calico.loadBalancerPools[]`, and pod pools are now rendered as explicit `crd.projectcalico.org/v1` `IPPool`s, so the full Flux `applications/calico/controlplane` IPv6-only setup is expressible. Spec: `docs/superpowers/specs/2026-09-19-ipv6-only-calico-design.md`; plan: `docs/superpowers/plans/2026-09-19-ipv6-only-calico.md`; live acceptance: `docs/runbooks/ipv6-only-kvm-verification.md`. Dual-stack is deliberately a future spec (address family is inferred from CIDRs, so relaxing the mixed-family check needs no API break).

**Non-obvious facts that drove the design (verified by rendering the charts, not assumed):**
- The tigera-operator chart **v3.29.1 ships 24 CRDs; v3.32.1 ships none.** From 3.32 the running operator creates every CRD (including all `crd.projectcalico.org` ones). Applying `Installation` right after the operator Deployment fails discovery with `Missing Kind` on 3.32, so the reconciler waits for each custom resource's kind via `apply::wait_for_kind_available` (180s, `timeout_at`-wrapped per [[wait-for-crd-established]]). The old `wait_for_crd_established` only helps chart-shipped CRDs.
- The operator skips creating pools from `Installation` when any `IPPool` exists (bring-up failure "no configured Calico pools"). Pod pools are therefore explicit objects **and** passed to `Installation` with the **same `name`**, so the operator adopts one pool instead of creating a duplicate. `IPPool` is one kind used by both pod and LB pools, so ordering is explicit phases in `reconcile`, not `rank_for_kind`.
- Calico's `IPPool.spec.cidr` is immutable: renumbering is a new-pool swap; retired pools stay declared with `disabled: true`.
- `blockSize` default is family-dependent (26 v4, 122 v6), so it is optional in the CRD and resolved at render time.

**Deliberately not done / still open:** the two live items in the runbook step 2 (no duplicate pool; `flexVolumePath` accepted by the 3.32.1 Installation CRD); chart hardening annotations and image digest pinning from the Flux app; readiness gating on `calico-node`. The cluster used for the example addressing must use a different AS and prefixes than any other cluster peering with the same gateway (placeholders: AS 64514, `fd00:db8:0:*`).

**How to apply:** when bumping the Calico chart version, re-render it (`helm template ... --include-crds`) and check whether CRD shipping changed before trusting the apply ordering; the ignored test `helm::tests::v3_32_1_chart_ships_no_crds` pins the current behavior.
```

Append to `docs/memory/MEMORY.md`:
```markdown
- [IPv6-only Calico slice](ipv6-only-calico-2026-09.md) — typed bgp/loadBalancerPools; chart v3.32.1 ships no CRDs so reconcile waits for kinds; pools need matching names
```

- [ ] **Step 4: Final verification**

Run: `cargo build && cargo test && cargo clippy --all-targets`
Expected: all PASS, no clippy errors. Then run the ignored network render tests:
`cargo test --lib helm::tests -- --ignored`
Expected: PASS (needs network and `helm`).

Confirm no real addressing leaked: `git grep -nE "fd97:45c2|2607:3640" -- examples tests src docs/runbooks`
Expected: only the intentional assertions in `src/helm.rs` tests (the existing `sample_spec` literal uses `fd97:45c2:b3a1`, pre-existing) and `tests/ipv6_example.rs`; nothing in `examples/` or `docs/runbooks/`.

- [ ] **Step 5: Commit**

```bash
git add docs/runbooks docs/memory
git commit -m "docs: add IPv6-only live verification runbook, memory and RBAC ledger updates" -m "Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```
