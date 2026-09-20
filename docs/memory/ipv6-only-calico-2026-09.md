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
