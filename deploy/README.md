# Deploying the platform controller

Apply in this order:

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/cloudcontrollermanagers.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/csidrivers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cni-installation.yaml
```

`crd.yaml` (all four CRDs) must be applied — and Established — first. `examples/cni-installation.yaml`
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

`bootstrap.yaml` uses `image: estenrye/platform-controller:0.1.3`
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

## Cloud controller manager (optional)

`examples/cloud-controller-manager.yaml` is a `CloudControllerManager` that
installs the OpenStack cloud controller manager on a self-hosted Talos cluster
running on OpenStack VMs. Skip it on a managed cluster (EKS, GKE, AKS, OKE): the
provider already runs one. It needs two things the controller cannot do for you:

- a one-time Talos machine-config change so kubelets use
  `--cloud-provider=external` (`docs/runbooks/cloud-controller-manager-verification.md`,
  step 0), and
- a Secret named as `spec.openstack.cloudConfigSecretRef.name` in `kube-system`,
  holding the OpenStack cloud config under the key `cloud.conf`. The controller
  never reads it, and does not check that it exists: with the Secret missing the
  DaemonSet's pod sits in `ContainerCreating` while `.status.phase` still says
  `Ready` (which means "manifests applied").

`spec.openstack.chartVersion` is the Helm chart version (`2.36.5`), not the
application version (`v1.36.0`).

**Apply order:** `CloudControllerManager`, then `CniInstallation`, then
`CsiDriver`, then `PullThroughCache`. The controller enforces no ordering; each
reconciles when applied. With external cloud-provider kubelets every node is tainted
`node.cloudprovider.kubernetes.io/uninitialized` until the CCM initializes it, and
the CCM runs on the host network, so it does not need the CNI.

**Upgrading an existing install:** apply the new `deploy/crd.yaml` (and wait for
all four CRDs to be Established) *before* rolling the controller image. The new
image's Deployment also tolerates the `uninitialized` taint (`deploy/bootstrap.yaml`);
without that toleration the controller could not schedule on a cluster whose
kubelets use an external cloud provider.

**Deleting** a `CloudControllerManager` removes the chart's objects but does not
undo node initialization (providerIDs and addresses stay) and does not delete
existing cloud load balancers.

## CSI driver (OpenStack Cinder, optional)

`examples/csi-driver-openstack-cinder.yaml` is a `CsiDriver` that installs the
`openstack-cinder-csi` driver (block storage) on a self-hosted Talos cluster
running on OpenStack VMs, giving the cluster two StorageClasses
(`csi-cinder-sc-delete`, `csi-cinder-sc-retain`). Skip it on a managed cluster
(EKS, GKE, AKS, OKE): the provider already runs its own CSI drivers.

Unlike `CloudControllerManager`, `CsiDriver` is **not a `name: default`
singleton**: a cloud can need several drivers installed at once (this
controller only builds OpenStack Cinder today), so each CR manages exactly one
driver and its name must equal that driver's own expected name --
`openstack-cinder` for `driver: openstackCinder`. A CR with any other name is
rejected (`Unsupported`).

This operator's CRD (`csidrivers.platform.rye.ninja`) shares its bare plural,
`csidrivers`, with Kubernetes' own built-in `storage.k8s.io` `CSIDriver`
resource -- which this exact chart also installs, as `cinder.csi.openstack.org`.
A bare `kubectl get csidrivers` resolves to the built-in resource, not this
one; use `kubectl get csi` (the shortname) or the fully-qualified
`csidrivers.platform.rye.ninja` instead.

The chart's `csi-snapshotter` sidecar crash-loops on this cluster (block-volume
provisioning is unaffected); see the runbook's step 2 for why and how to
silence it.

It needs a Secret named as `spec.openstackCinder.cloudConfigSecretRef.name` in
`kube-system`, holding the OpenStack cloud config under the key `cloud.conf`.
The controller never reads it, and does not check that it exists: with the
Secret missing the driver's pods sit in `ContainerCreating` while
`.status.phase` still says `Ready` (which means "manifests applied").

`spec.openstackCinder.chartVersion` is the Helm chart version (`2.36.5`), not
the application version (`v1.36.0`). `spec.openstackCinder.defaultStorageClass`
(`delete`, `retain` or `none`; default `delete`) picks which StorageClass, if
any, is the cluster default.

**Apply order:** `CloudControllerManager`, then `CniInstallation`, then
`CsiDriver`, then `PullThroughCache`. The controller enforces no ordering, but
unlike the CCM's DaemonSet, the CSI driver's controller-plugin Deployment runs
on the regular pod network and has no toleration for the `uninitialized` taint
-- it needs both the CNI and the CCM's node initialization to actually run,
even though the reconcile that applies its manifests will succeed regardless.

**Upgrading an existing install:** apply the new `deploy/crd.yaml` (and wait
for all four CRDs to be Established) *before* rolling the controller image.

**Deleting** a `CsiDriver` removes the chart's objects but does not delete
already-provisioned Cinder volumes; PVCs or pods still depending on them can be
left with a stuck detach/unmount.

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
