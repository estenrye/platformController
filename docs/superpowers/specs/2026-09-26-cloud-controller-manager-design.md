# Cloud Controller Manager (OpenStack) on Talos

Status: Draft, awaiting review
Date: 2026-09-26

## Purpose

A self-hosted Kubernetes cluster running on a cloud's VMs has no component that ties its `Node` objects to the cloud's instances. Without a **cloud controller manager (CCM)**, nodes have no `providerID`, their addresses come only from the kubelet, a node whose VM has been deleted stays in the cluster forever, and Services of type `LoadBalancer` never get an external address. This spec adds a third platform component to the controller: a `CloudControllerManager` resource, first backed by [cloud-provider-openstack](https://github.com/kubernetes/cloud-provider-openstack) for Talos clusters running on OpenStack VMs.

The API is shaped so that the other cloud providers can be added as further `provider` values without reshaping the CRD: [cloud-provider-aws](https://github.com/kubernetes/cloud-provider-aws), [cloud-provider-gcp](https://github.com/kubernetes/cloud-provider-gcp), [cloud-provider-azure](https://github.com/kubernetes-sigs/cloud-provider-azure) and [oci-cloud-controller-manager](https://github.com/oracle/oci-cloud-controller-manager). None is built here.

A CCM only applies where the operator runs the control plane. On EKS, GKE, AKS and OKE the provider already runs its own CCM, so the managed `platformKind` values in `docs/goals.md` will not get this component.

## Non-goals

- Cinder or Manila CSI. Storage is a separate component.
- The AWS, GCP, Azure and OCI providers.
- Applying Talos machine config. The controller speaks only the Kubernetes API (see the node prerequisite below).
- Typed `cloud.conf` fields (networking, load balancer, metadata). Non-secret tuning goes through `helmValues`.
- Reading or checking the cloud-config Secret. The controller never reads its contents and does not check that it exists.
- A health-derived status condition. `Ready` means "manifests applied", exactly as it does for the other two components.
- An airgapped chart source. The controller fetches the chart from `kubernetes.github.io`, so it needs that egress.
- Refactoring the three reconcilers into a shared component framework. This slice adds the third copy of the finalizer and status glue; that is the evidence for a later, separate project.

## Node prerequisite (documented, not automated)

Every node's kubelet must run with `--cloud-provider=external`, and the control-plane components must be configured for an external cloud provider. Talos exposes this as `cluster.externalCloudProvider.enabled: true` in the machine config. The controller cannot apply it and cannot verify it; it reports only that its own manifests were applied. The exact patch is recorded in `docs/runbooks/cloud-controller-manager-verification.md` and verified against the Talos version in use.

With that set, each node registers tainted `node.cloudprovider.kubernetes.io/uninitialized:NoSchedule`, and stays tainted until the CCM initializes it.

## Design

### API

A new cluster-scoped singleton CRD in the existing group, next to `CniInstallation` and `PullThroughCache`:

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CloudControllerManager   # cluster-scoped, shortname "ccm"
metadata:
  name: default                # singleton, same rule as the other two
spec:
  platformKind: talos-linux    # only value accepted today
  provider: openstack          # enum; aws, gcp, azure, oci come later
  openstack:
    chartVersion: "2.36.5"     # required, no default
    cloudConfigSecretRef:
      name: cloud-config       # required; Secret in kube-system holding cloud.conf
    helmValues: {}             # optional free-form passthrough
```

- `chartVersion` is the Helm **chart** version (`2.36.5`), not the application version (`v1.36.0`). It is passed verbatim to `helm --version`.
- `cloudConfigSecretRef.name` names a Secret in `kube-system`. The namespace is not configurable: the chart mounts the Secret from its own release namespace. The Secret must hold the whole cloud config under the key **`cloud.conf`**; the chart's DaemonSet reads `/etc/config/cloud.conf`. The controller passes only the name to the chart. It never reads the Secret, so nothing secret appears in the CR, its status or the ledger.
- Values layering, as for `PullThroughCache`: `helmValues` first, then the typed fields (`secret.enabled: true`, `secret.create: false`, `secret.name: <ref>`), then the controller's unconditional Talos overrides. A passthrough can never contradict a typed field or a platform-implied value.
- For `talos-linux` the controller always sets `extraVolumes: []` and `extraVolumeMounts: []`. The chart's defaults hostPath-mount `/etc/kubernetes/pki` and the kubelet flexvolume directory, which the CCM does not use (it uses in-cluster config) and which Talos does not provide. This is the same kind of unconditional platform-implied value as Calico's `flexVolumePath: None`. Not verified live; see the runbook.
- The controller always sets `dnsPolicy: Default`. The chart defaults to `hostNetwork: true` with `dnsPolicy: ClusterFirstWithHostNet`, which points the pod at the cluster DNS service IP. That IP is unreachable before a CNI is up, and CoreDNS itself cannot schedule until the CCM clears the `uninitialized` taint from every node — the same deadlock `deploy/bootstrap.yaml` already documents for this controller's own Deployment (see its "Default, not ClusterFirstWithHostNet" comment). `Default` inherits the node's resolv.conf, which resolves the cloud's Keystone endpoint with no pod network required. Set unconditionally, so a `helmValues` passthrough can never reintroduce the deadlock.
- No `cleanupTimeoutSeconds`: the CCM has no operator-owned resources that need a bounded wait on removal.
- Deliberately not typed: `clusterName` and `enabledControllers`. They remain reachable through `helmValues`. The `cloudConfig`/`cloudConfigContents` block is **not** reachable through `helmValues`, and setting it there is rejected (`InvalidHelmValues`): `secret.create` is always forced to `false` (see above), so the chart's Secret template, the only consumer of `cloudConfig`/`cloudConfigContents`, never renders — the values would have no effect, and a user who set them there would have OpenStack credentials sitting, uselessly, in a cluster-scoped custom resource that is not a Secret. The credential path is exactly one: `cloudConfigSecretRef`.

Validation, rejected with `phase: Failed` and a `reason`, like the other paths:

- the name is not `default`
- `platformKind` or `provider` is unsupported
- `chartVersion` is empty
- `chartVersion` has leading or trailing whitespace
- `cloudConfigSecretRef.name` is empty or is not a valid Kubernetes object name (DNS-1123 subdomain)
- `helmValues` is not a JSON object
- `helmValues` sets `cloudConfig` or `cloudConfigContents` (see above)

Status mirrors `PullThroughCacheStatus` and reuses `Phase`, `Condition` and `AppliedResourceRef`: `phase`, `observedGeneration`, `chartVersion`, `appliedResources`, `conditions`.

### Reconcile

Same shape as the Spegel loop:

1. Leader gate, then validate; write `Failed` status on rejection.
2. Render the chart with `helm template` from the classic repo `https://kubernetes.github.io/cloud-provider-openstack`, chart `openstack-cloud-controller-manager`, namespace `kube-system`, `--no-hooks --include-crds`. The existing `--repo` render form is reused; no new render path is expected (to be confirmed while planning).
3. Parse, sort and apply the rendered objects in rank order, then prune anything no longer rendered against `status.appliedResources`. The ledger is checkpointed before applying and failures after validation write status, as in `reconciler-status-and-ledger`. No namespace is synthesized: `kube-system` already exists and Talos's default admission configuration exempts it, so the hostNetwork DaemonSet is admitted.
4. Write `Ready` status; requeue at 300s.

Observed by rendering chart `2.36.5` with `secret.create=false`: a ServiceAccount, a ClusterRole and binding, a Role and binding (`secret-reader`, scoped by `resourceNames` to the Secret's name), and one DaemonSet (`hostNetwork: true`, node selector `node-role.kubernetes.io/control-plane`, tolerating `node.cloudprovider.kubernetes.io/uninitialized`). No Secret object is rendered.

### Missing or wrong Secret

The controller does not check the Secret. If it is absent the DaemonSet's pod stays in `ContainerCreating` while status still reports `Ready`. This is documented in the runbook. A metadata-only existence check that reports a `Failed` reason is a possible follow-up; it is out of scope here because it makes the controller start reading Secrets.

### The `route` controller

The chart enables `cloud-node`, `cloud-node-lifecycle`, `route` and `service`. The `route` controller programs Neutron routes for pod CIDRs and may conflict with Calico owning pod routing. This spec leaves `enabledControllers` at the chart default and treats the behavior as a live-verification item. If it conflicts, the fix is an unconditional `enabledControllers` override without `route`, and this section changes.

### Ordering and the controller's own Deployment

The CCM is `hostNetwork` and tolerates the uninitialized taint, so it does not need the CNI. The controller's own Deployment does not tolerate that taint, so on a cluster whose kubelets use an external cloud provider it could never schedule and nothing could install the CCM. `deploy/bootstrap.yaml` therefore gains a toleration for `node.cloudprovider.kubernetes.io/uninitialized` (`operator: Exists`, effect `NoSchedule`). This is the one required change outside the new component.

The controller enforces no ordering between the three resources; each reconciles when applied. The deploy README documents the order: `CloudControllerManager`, then `CniInstallation`, then `PullThroughCache`.

### Cleanup

The finalizer deletes applied resources in reverse ledger order. There are no removal waits. Deleting the CCM does not undo node initialization: `providerID`s and addresses stay. `cloud-node-lifecycle` simply stops removing nodes whose VM has disappeared, and `LoadBalancer` Services stop being reconciled (existing cloud load balancers are not deleted by this). The runbook records this.

### Open interaction: IPv6 node addresses

`CniInstallation` can use each node's Kubernetes InternalIP for IPv6 autodetection (`nodeAddressAutodetectionV6Method: kubernetesInternalIP`). Once a CCM initializes nodes it can rewrite node addresses from Neutron. Whether that changes what Calico sees is not designed around here; it is a live-verification item.

### Wiring

- `main.rs` builds a third watcher and `Controller<CloudControllerManager>` with the same generation, deletion-requested and finalizer predicate filter, in the existing `select!`, sharing one `Context` and one lease.
- The finalizer name (`platform.rye.ninja/cleanup`) is reused: finalizers are per object.
- `leader_gate` and `wait_for_object_kind` are already `pub` in `reconciler.rs` and are reused. The finalizer and status glue is duplicated from `cache_reconciler.rs`. The CNI and cache paths are otherwise untouched.

### Code layout

- `src/cloud_controller_manager.rs`: CRD types, name validation and the values builder.
- `src/ccm_reconciler.rs`: reconcile, cleanup, finalizer wiring, status.
- `src/main.rs`: third controller.
- `src/bin/crdgen.rs`: emits all three CRDs.

### Deployment and docs

- `deploy/crd.yaml` regenerated with all three CRDs.
- `deploy/bootstrap.yaml`: the toleration above. `deploy/README.md` waits for `established` on all three CRDs and states the apply order. No RBAC change (the controller is cluster-admin); the RBAC ledger memory gains rows.
- `examples/cloud-controller-manager.yaml`: a Talos-on-OpenStack starting point, with a comment showing the shape of the `cloud.conf` Secret.
- `docs/runbooks/cloud-controller-manager-verification.md`: the machine-config prerequisite and the live checks below.
- A `docs/memory/` entry and index line, per CLAUDE.md.

## Testing

- **Unit:** spec deserialization and defaults; each validation rejection; values builder (typed fields win over `helmValues`, the Secret name is passed through, the volume overrides are always set, `secret.create` is `false`).
- **Real-chart (ignored, needs network), in the style of the Spegel real-chart test:** render chart `2.36.5` with the controller's values and assert no Secret object, exactly one DaemonSet, the mount key `cloud.conf`, and no hostPath volumes. Re-run on every chart bump.
- **Example:** the example manifest parses and validates, in the style of `tests/ipv6_example.rs`.
- **Integration (ignored, Talos-in-Docker):** apply the CR, assert the DaemonSet appears, delete the CR, assert the applied resources are gone. It does not assert readiness, since the pod cannot run without OpenStack credentials.
- **Live acceptance (manual, real OpenStack, runbook):**
  - nodes initialized: `providerID` set and the uninitialized taint cleared
  - node addresses, including the IPv6 interaction above
  - a `LoadBalancer` Service receiving an address
  - the `route` controller's behavior under Calico
  - the missing-Secret failure mode
  - delete behavior, as described in Cleanup
  - the controller pod scheduling under the new toleration
  - the `extraVolumes: []` override working on Talos

## Verification status

Not yet implemented. Verified so far, by rendering the real chart on 2026-09-26: chart `2.36.5` (app `v1.36.0`) exists on the classic repo, renders the objects listed under Reconcile with `secret.create=false`, and mounts the Secret by name with the file `cloud.conf`. Everything about behavior on a live cluster is unverified.
