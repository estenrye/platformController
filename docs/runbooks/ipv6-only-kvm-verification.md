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
