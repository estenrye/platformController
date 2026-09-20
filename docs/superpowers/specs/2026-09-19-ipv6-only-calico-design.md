# IPv6-Only Calico on Talos: Full Flux Parity via `CniInstallation`

Status: Draft for review
Date: 2026-09-19

## Purpose

Make `CniInstallation` able to express everything the Flux
`applications/calico/controlplane` app configures for an IPv6-only Talos
cluster: the operator install, explicit pod pools, BGP configuration and
peering, and LoadBalancer VIP pools. Ship an IPv6-only example manifest and a
live verification runbook.

The existing MVP ([design](2026-09-18-calico-talos-mvp-controller-design.md))
already renders the tigera-operator chart and its `Installation`/`APIServer`
CRs. It cannot express `BGPConfiguration`, `BGPPeer`, LoadBalancer-only
`IPPool`s, or explicit pod `IPPool`s, and it pins an older chart.

## Non-goals

- Dual-stack. Mixed address families in one installation are rejected for
  now; dual-stack support is planned as its own future spec. Because address
  family is inferred from the CIDRs (Section 1) and not from a flag, that spec
  can relax the mixed-family check without an API break.
- Other Calico kinds (`FelixConfiguration`, etc.) and any raw-manifest
  passthrough. New kinds get typed fields when needed.
- Chart hardening carried in the Flux app: kube-linter/checkov annotations,
  Deployment resource/securityContext patches, image digest pinning. Digest
  pinning can arrive later via the `helmValues` escape hatch in
  [goals.md](../../goals.md).
- The `tigera-operator-uninstall` Job patch; `--no-hooks` already covers it.
- Gating `status.phase = Ready` on `calico-node` readiness.
- Gateway (UniFi/FRR) configuration. It is a runbook prerequisite, not managed
  here.

## 1. CRD API (additive)

Existing resources keep working. New and changed fields under `spec.calico`:

```yaml
spec:
  platformKind: talos-linux
  provider: calico
  calico:
    chartVersion: v3.32.1
    bgpEnabled: true
    apiServerEnabled: true
    ipPools:
      - name: pods-v6
        cidr: fd00:db8:0:1100::/56
        encapsulation: None
        natOutgoing: true
        blockSize: 122            # optional; defaults by CIDR family
    nodeAddressAutodetectionV6Cidrs: ["fd00:db8:0:179::/64"]
    bgp:                          # NEW; requires bgpEnabled: true
      asNumber: 64514
      nodeToNodeMeshEnabled: true
      logSeverityScreen: Info
      serviceLoadBalancerIPs:
        - fd00:db8:0:f00::/112
        - 2001:db8:0:27f::/112
      peers:
        - name: gateway
          peerIP: fd00:db8:0:179::1
          asNumber: 64512
    loadBalancerPools:            # NEW; allowedUses [LoadBalancer] only
      - name: lb-internal-routed
        cidr: fd00:db8:0:f00::/112
        nodeSelector: "all()"
        disabled: false
```

- **Address family is inferred**, not a flag. No IPv4 pool and no
  `nodeAddressAutodetectionV4` means the operator never enables IPv4.
- **`blockSize` becomes optional.** Resolved at render time: 26 for IPv4 CIDRs,
  122 for IPv6 CIDRs. (Today's fixed default of 26 is wrong for an IPv6 pool
  that omits it.)
- **Retired pools stay declared with `disabled: true`.** Calico's pool CIDR is
  immutable, so renumbering is a new-pool swap, not an in-place edit.
- **Chart repository** default changes to `https://docs.tigera.io/calico/charts`
  (what the Flux app uses). No new field; `chartVersion` remains required and
  examples move to v3.32.1. Native LoadBalancer IPAM requires Calico 3.30+.

### Addressing is entirely spec-driven

The AS number and every CIDR are spec fields; nothing is hardcoded. The
example uses neutral placeholders, chosen so a second cluster can coexist with
an existing one:

| Placeholder | Meaning | Replaces (Flux app) |
|---|---|---|
| AS `64514` | cluster AS | `64513` (an existing active cluster) |
| `fd00:db8:0:1100::/56` | pod pool | separate ULA `/56` |
| `fd00:db8:0:179::/64` | BGP peering segment; peer `::1` is the gateway | VLAN 179 `/64` |
| `fd00:db8:0:f00::/112` | internal VIP pool | internal ULA VIP `/112` |
| `2001:db8:0:27f::/112` | ingress VIP pool (documentation range) | ingress GUA `/112` |

Real values are substituted at apply time and are not committed to this repo.
They must not overlap with any active cluster's pod, peering or VIP ranges.

## 2. Validation

`validate()` runs before any apply, so a bad spec never leaves a half-configured
cluster. Each rejection yields `phase: Failed` with a specific condition reason:

- Every CIDR parses, and all CIDRs in one installation share one family.
- `blockSize` is in range for the pool's family (20-32 IPv4, 116-128 IPv6).
- `bgp` is set only when `bgpEnabled: true`.
- Pool and peer names are unique; `serviceLoadBalancerIPs` entries are CIDRs.
- `nodeAddressAutodetectionV6Cidrs` is set only for an IPv6 installation.
- `encapsulation: IPIP` is rejected on an IPv6 pool (Calico does not support
  IPIP over IPv6).

## 3. Reconcile flow and ordering

The controller builds the Calico objects (`crd.projectcalico.org/v1`) itself,
as `DynamicObject`s from `spec.calico`, alongside the Helm render. Apply happens
in explicit phases:

| Phase | Objects | Notes |
|---|---|---|
| 0 | `Namespace` | Unchanged. Already carries the `pod-security.kubernetes.io/*: privileged` labels the hostNetwork operator pod needs. |
| 1 | Chart objects, in `rank_for_kind` order | Unchanged. Includes chart CRDs when the chart ships them (v3.29.x) and the chart's custom resources (`Installation`, `APIServer`, and on v3.32.x also `Goldmane`, `Whisker`) last. |
| 2 | Pod `IPPool`s | Explicit, from `spec.calico.ipPools`: `allowedUses: [Workload, Tunnel]`, `ipipMode`/`vxlanMode` derived from `encapsulation`. |
| 3 | `BGPConfiguration`, `BGPPeer`, LB `IPPool`s | Applied only after pod pools exist. |

Phases 2 and 3 are explicit steps in `reconcile`, not `rank_for_kind` values,
because pod pools and LB pools are the same kind (`IPPool`) at different
phases. Every Calico object falls in the existing "custom resource" bucket for
cleanup purposes.

**Why explicit pod pools.** The operator skips creating pools from
`Installation` when any `IPPool` already exists. The Flux app hit this at
bring-up (every pod stuck with "no configured Calico pools") and declares its
pod pool explicitly. The controller does the same, so ordering doesn't depend
on operator timing. Pod pools are still passed in `Installation` too, so the
operator's own validation sees them.

**Chart v3.32.1 ships no CRDs.** Verified by rendering both charts with
`--include-crds`: v3.29.1 emits 24 `CustomResourceDefinition`s (including every
`crd.projectcalico.org` one); v3.32.1 emits none. From 3.32 the running
operator creates all CRDs (`operator.tigera.io` and `crd.projectcalico.org`)
at startup. The current flow applies `Installation` immediately after the
operator `Deployment` and would fail discovery with `Missing Kind` on 3.32.

**Generic kind-availability wait.** Before applying any object whose kind is
not a built-in of the chart (every phase 1 custom resource, and every phase 2/3
object), the controller polls API discovery until that `apiVersion/kind`
resolves, bounded by a deadline. This covers both chart generations: instant
when the CRD already exists (3.29.x, or a re-reconcile), and a real wait while
the operator starts, pulls its image and registers CRDs (3.32.x). The deadline
is 180s, since a first install on a CNI-less cluster includes an image pull.
Per [[wait-for-crd-established]], every API call in the poll is wrapped in
`tokio::time::timeout_at`, not only the loop. On timeout the reconcile fails
with `KindNotAvailable` and retries via the normal `error_policy` backoff. The
existing `wait_for_crd_established` stays for chart-shipped CRDs.

**Prune and cleanup reuse existing mechanisms.** New objects are recorded in
`status.appliedResources`, so removing a peer or pool from the spec prunes it.
On delete, `partition_for_cleanup` already classifies unknown kinds as custom
resources and deletes them (in reverse apply order) before the operator, so
BGP/LB objects go first, then pod pools, then `Installation`. `IPPool`
deletion while pods hold IPAM blocks is documented as known behavior; no
special handling. On v3.32.x the CRDs are operator-created and are not tracked
by the controller, so they remain after cleanup (same as `helm uninstall`).

### Open verification items (live, in the runbook)

1. That `Installation.ipPools` plus an explicit `IPPool` with the same CIDR do
   not conflict. The Flux app runs exactly this configuration.
2. That the v3.32.1 `Installation` CRD accepts `flexVolumePath: None` without
   an unknown-field warning (the chart itself also renders
   `kubeletVolumePluginPath: None`). If it warns, drop `flexVolumePath` for
   chart versions that no longer define it.

## 4. Example, tests, runbook

**Example:** `examples/cni-installation-ipv6.yaml`, the CR equivalent of the
Flux `controlplane` app, with the placeholder table above and comments mapping
each block to its Calico object. The existing IPv4 example is unchanged.

**Offline tests (CI):**

- CRD round-trip of the IPv6 example, including family-based `blockSize`
  defaults.
- Golden tests: spec to rendered `IPPool`, `BGPConfiguration`, `BGPPeer` and LB
  pool objects, compared to fixtures derived from the Flux YAML with the
  placeholder addressing.
- Rank ordering: pod pools before LB/BGP objects; `Installation` values contain
  no IPv4 keys.
- Validation table tests for every rejection in Section 2.
- The kind-availability wait's pure classifier (which `kube::Error`s mean
  "kind not registered yet"), shared with `dynamic_api_for`. As with
  `wait_for_crd_established`, the polling loop itself is covered live, not by
  a fake client.
- `deploy/crd.yaml` regenerated via `crdgen` and checked for drift.

**Live verification (acceptance gate):**
`docs/runbooks/ipv6-only-kvm-verification.md`, an opt-in checklist against a
KVM Talos cluster. CI cannot simulate the gateway peering. Steps:

0. Prerequisite: gateway FRR accepts the new cluster AS and peering `/64` as a
   dynamic neighbor.
1. Apply the CR; nodes reach `Ready` with only IPv6 addresses.
2. BGP sessions establish with the gateway; VIP pools are advertised.
3. Pod-to-external traffic is SNAT'd to the node address.
4. Delete the CR; cleanup completes.

## 5. Housekeeping

- Update the permission ledger in `docs/memory/rbac-cluster-admin-tradeoff.md`
  with the new `crd.projectcalico.org` kinds (cluster-admin already covers
  them; the ledger tracks them for a future scope-down).
- Add a project memory for this slice under `docs/memory/` and index it in
  `docs/memory/MEMORY.md`.
