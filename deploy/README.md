# Deploying the platform controller

Apply in this order:

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cni-installation.yaml
```

`crd.yaml` (both CRDs) must be applied — and Established — first. `examples/cni-installation.yaml`
contains a `CniInstallation` custom resource, and the API server rejects a custom
resource whose kind is not yet registered (`no matches for kind "CniInstallation"`).
Registration is asynchronous: the CRD can exist while its API endpoint is not yet
serving, so waiting on `condition=established` (rather than just applying the files
back to back) is what makes that apply reliable.

`bootstrap.yaml` installs the controller itself (namespace, RBAC, Deployment,
PodDisruptionBudget) but does not include a `CniInstallation` — that's a separate
configuration step, since it's specific to your cluster's platform/CNI/network
plan. `examples/cni-installation.yaml` is a starting point for a self-hosted Talos
Linux cluster running Calico; copy and adapt it (CIDR, encapsulation, BGP, etc.)
to your own cluster rather than applying it as-is on anything but a test cluster.

`bootstrap.yaml` uses `image: estenrye/platform-controller:0.1.2`
(`imagePullPolicy: IfNotPresent`), a published image on Docker Hub — most clusters
with normal internet access can apply `bootstrap.yaml` as-is with no further steps.
For an airgapped cluster, or one that otherwise can't reach Docker Hub, build the
image locally and make it available via a registry the cluster can reach instead
(see `tests/integration_talos.rs`'s header comment for a worked example using a
local registry and Talos's `--registry-mirror`):

```sh
docker build -t platform-controller:latest .
```

Regenerate `crd.yaml` after any change to the CRD types:

```sh
cargo run --bin crdgen > deploy/crd.yaml
```

## Pull-through image cache (optional)

`examples/pull-through-cache.yaml` is a `PullThroughCache` that installs
[Spegel](https://spegel.dev), a peer-to-peer image mirror, on Talos. Apply it
**after** the CNI is `Ready` (the chart runs Spegel on the pod network and
discovers peers through cluster DNS), and only
after doing the one-time Talos machine-config change described in
`docs/runbooks/pull-through-cache-verification.md` (step 0); the controller cannot
make that change for you.

Omitting `spec.spegel.registries` mirrors every registry, private ones included.
`spec.spegel.chartVersion` is the OCI chart tag and has no `v` prefix (`0.7.4`).

**Upgrading an existing install:** apply the new `deploy/crd.yaml` (and wait for
both CRDs to be Established) *before* rolling the controller image. A controller
that starts without the `PullThroughCache` CRD logs watch errors for it and
retries with backoff; it still reconciles `CniInstallation` normally.

## Calico node address autodetection

`spec.calico.nodeAddressAutodetectionV6Method` selects how Calico detects each
node's IPv6 address: `cidrs` (the default) uses `nodeAddressAutodetectionV6Cidrs`,
and `kubernetesInternalIP` uses each node's Kubernetes InternalIP. Use the latter
when the peering `/64` carries other addresses (SLAAC addresses, a floating VIP),
which makes CIDR autodetection ambiguous. The two settings cannot be combined.

**Upgrading:** apply the new `deploy/crd.yaml` *before* rolling the controller
image; otherwise the API server silently prunes the unknown field from a manifest
that uses it. **Switching an existing cluster** from `cidrs` to
`kubernetesInternalIP` changes each node's address in Calico: `calico-node`
restarts and BGP sessions re-establish from the new addresses, so make sure your
BGP peers accept them first.
