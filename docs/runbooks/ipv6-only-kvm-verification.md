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

The peering segment must actually exist on the gateway: a VLAN subinterface, a
bridge, and the gateway's own address (the `peerIP`) on that bridge. Configuring
FRR alone is not enough; without the address the node's neighbor entry for the
peer stays `INCOMPLETE` and the BGP session sits in `Connect` ("No route to
host").

### Hypervisor prerequisites (OpenStack or similar)

Virtual networks filter traffic per port, so on the node's port on the peering
segment:

- allow ingress (IPv6 ethertype) for every LoadBalancer service port you will
  test, from the client networks. Only ports with a rule are reachable (the
  VIP arrives on the service port, not a NodePort);
- add every `loadBalancerPools` CIDR as an allowed address pair. Without it the
  VIP is dropped in both directions, even when the port is open.

The node needs a single NIC on the peering segment, or symmetric routing if it
has two: a reply must leave the NIC the request arrived on. A second NIC with a
lower-metric default route sends VIP replies out the wrong port, where they are
dropped (source rules cannot fix this, because the reply is routed before the
NAT rewrites its source to the VIP).

## 1. Install

Run a controller image built from the branch under test. `deploy/bootstrap.yaml`
points at `:latest`, which may predate the Calico phases; a controller that
does not know them installs only the operator and creates no BGP peer or LB
pools, and the operator then creates its own pool alongside yours. Set the
image before applying the `CniInstallation`.

```bash
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl -n platform-system set image deploy/platform-controller platform-controller=<your image>
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

`status.appliedResources` is checkpointed before anything is applied (see docs/superpowers/specs/2026-09-25-reconciler-status-and-ledger-design.md), so a first install that fails partway (for example `KindNotAvailable`) and is then deleted is cleaned up like any other install. A failure after validation also appears on the resource: `kubectl get cni default -o jsonpath='{.status.phase}{" "}{.status.conditions[0].reason}{"\n"}'` shows `Failed` with `RenderFailed`, `InvalidManifest` or `ApplyFailed`. Live-verified 2026-09-25 on a single-node Talos cluster (controller run locally against it): an existing installation reconciled with no checkpoint write and stayed `Ready`; setting a nonexistent `spec.calico.chartVersion` (`v9.9.9`) showed `Failed` / `RenderFailed` with the helm error and the full ledger preserved, every Calico pod untouched and the node still `Ready`, and restoring `v3.32.1` returned it to `Ready` with the same pools. The checkpoint-before-apply path itself was exercised live on `PullThroughCache` only; a CNI first install that fails partway was not reproduced live.

## 2. Open verification items

These two behaviors were unverifiable offline. Record the outcome in the PR.

```bash
# (a) Exactly one pool per declared name; no operator-created duplicate.
kubectl get ippools.crd.projectcalico.org
# Expected: pods-v6, lb-internal-routed, lb-ingress-routed (and nothing else),
# on a clean install with the controller image already in place. A pool named
# default-ipv6-ippool means the operator created one before the controller did.

# (a2) LB pools are written directly to crd.projectcalico.org/v1 with only
# cidr/allowedUses/nodeSelector/disabled (no ipipMode/vxlanMode/natOutgoing/
# blockSize, unlike pod pools); confirm the stored object.
kubectl get ippools.crd.projectcalico.org lb-internal-routed -o yaml
# Expected: the pool is present with allowedUses [LoadBalancer].

# (b) The v3.32.1 Installation CRD still defines flexVolumePath. The API server
# prunes fields the schema does not define (any warning goes to the client in a
# response header, not to the controller logs), so check the stored object.
kubectl get installation default -o jsonpath='{.spec.flexVolumePath}'
# Expected: None. Empty output means the CRD pruned the field: stop and drop
# flexVolumePath for chart versions that no longer define it.
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
kubectl get svc vip-test        # EXTERNAL-IP is inside one of your LB pools
vtysh -c 'show bgp ipv6 unicast <EXTERNAL-IP>'       # on the gateway
curl -g "http://[<EXTERNAL-IP>]/"                    # from a LAN client
```

Expected: the VIP is allocated from one of your LB pools (either can be
picked), covered by a route in the gateway's BGP table, and reachable. The
gateway holds the advertised `/112` range, not a `/128`, so look the address up
without a prefix length: a `<EXTERNAL-IP>/128` lookup reports "Network not in
table". Test from the gateway and from a client on another network: if only
the gateway works, suspect the return path (see step 0). Clean up:
`kubectl delete svc,deploy vip-test`.

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

Expected: the delete completes on its own within about a minute (finalizer
removed), with no manual `kubectl delete` of any operator resource, and
`tigera-operator` and `calico-system` are gone. The controller deletes Calico
objects (BGP, LB pools, pod pools) without waiting, deletes the operator's
`APIServer`, `Goldmane`, `Whisker` and finally `Installation` (the
`Installation` holds finalizers that wait for the others), waits for them, then
deletes the Calico objects once more: the operator recreates a deleted pod pool
while the `Installation` still exists, so the sweep removes it after. Afterwards
`kubectl get ippools.crd.projectcalico.org` must show no pools. On chart
v3.32.x the operator-created CRDs remain after cleanup (same as
`helm uninstall`); that is expected.

Cleanup does not touch CoreDNS. Its pods keep pod IPs that no longer route
after Calico is removed; restart them (`kubectl -n kube-system rollout restart
deploy/coredns`) once the next install is Ready.

## Record

Note the chart version, Talos version, and the result of each step in the PR
description.
