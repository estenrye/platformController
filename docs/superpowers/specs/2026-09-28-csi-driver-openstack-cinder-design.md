# CSI Driver (OpenStack Cinder) on Talos

Status: Draft, awaiting review
Date: 2026-09-28

## Purpose

A self-hosted Kubernetes cluster has no `StorageClass` that can provision volumes from the underlying cloud until a CSI driver is installed for that cloud's block (or file, or object) storage service. This spec adds a fourth platform component to the controller: a `CsiDriver` resource, first backed by [cinder-csi-plugin](https://github.com/kubernetes/cloud-provider-openstack/blob/master/docs/cinder-csi-plugin/using-cinder-csi-plugin.md) for Talos clusters running on OpenStack VMs.

Unlike `CniInstallation` (exactly one CNI plugin active) and `CloudControllerManager` (exactly one chart per cloud), a single cloud provider can need *several* CSI drivers running at once — for example AWS wants both `ebs.csi.aws.com` and `efs.csi.aws.com` installed simultaneously, not a choice between them. Rather than teach the reconciler to render and apply multiple charts per custom resource, `CsiDriver` keeps the existing "one chart per CR, one CR per thing" shape and instead allows **multiple CR instances**, one per driver. The API is shaped so that other drivers — one per cloud/storage-type combination — can be added as further `driver` enum values without reshaping the CRD or the reconciler; none of them is built here.

A CSI driver only applies where the operator runs the control plane. On EKS, GKE, AKS and OKE the provider already ships its own CSI drivers (often as a managed add-on), so the managed `platformKind` values in `docs/goals.md` will not get this component; see "Future providers" below for how each would eventually map onto this same shape.

## Non-goals

- Manila (OpenStack's file-storage CSI driver, `openstack-manila-csi` in the same chart repo as Cinder). Only block storage is built here.
- The AWS, Azure, GCP and OCI drivers.
- Typed control over StorageClass parameters (availability zone, volume type, `mkfs` options) beyond which one is the cluster default. Reachable through `helmValues`.
- Reading or checking the cloud-config Secret. The controller never reads its contents and does not check that it exists, matching `CloudControllerManager`.
- A health-derived status condition. `Ready` means "manifests applied", exactly as it does for the other three components.
- Volume snapshot support beyond what the chart enables by default (`csi.snapshotter.enabled: true` renders the snapshotter sidecar, but the cluster-wide `snapshot-controller` and the `VolumeSnapshotClass` CRDs are a separate, cluster-level concern this component does not install).
- An airgapped chart source. The controller fetches the chart from `kubernetes.github.io`, so it needs that egress.
- Refactoring the four reconcilers (`CniInstallation`, `PullThroughCache`, `CloudControllerManager`, `CsiDriver`) into a shared component framework. This slice adds a fourth copy of the finalizer and status glue; the case for extraction, already flagged after the third copy, is stronger now but still out of scope here.

## Design

### API

A new cluster-scoped CRD in the existing group, next to `CniInstallation`, `PullThroughCache` and `CloudControllerManager` — but, unlike those three, **not a `name: default` singleton**. Each instance manages one driver, and its name must equal that driver's canonical name, so at most one CR can ever manage a given driver:

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CsiDriver                      # cluster-scoped, shortname "csi"
metadata:
  name: openstack-cinder             # must equal the kebab-case form of spec.driver
spec:
  platformKind: talos-linux          # only value accepted today
  driver: openstackCinder            # enum; only value accepted today
  openstackCinder:
    chartVersion: "2.36.5"           # required, no default
    cloudConfigSecretRef:
      name: cloud-config             # required; Secret in kube-system holding cloud.conf
    defaultStorageClass: delete      # enum: delete | retain | none; default "delete"
    helmValues: {}                   # optional free-form passthrough
```

- `chartVersion` is the Helm **chart** version (`2.36.5`), not the application version (`v1.36.0`). Passed verbatim to `helm --version`, same convention as `CloudControllerManager`.
- `cloudConfigSecretRef.name` names a Secret in `kube-system`, holding the whole cloud config under the key **`cloud.conf`** (the chart's default `secret.filename`). The controller passes only the name to the chart: `secret.enabled: true`, `secret.create: false`, `secret.hostMount: false`, `secret.name: <ref>`. It never reads the Secret.
- Values layering, as for `CloudControllerManager`: `helmValues` first, then the typed fields above, so a passthrough can never contradict a typed field.
- `helmValues` must not set `secret.data`: `secret.create` is always forced `false`, so the chart's Secret template — the only consumer of `secret.data` — never renders. A user who set it there would have OpenStack credentials sitting, uselessly, in a cluster-scoped custom resource that is not a Secret. Rejected as `InvalidHelmValues`, the same rationale `CloudControllerManager` already applies to `cloudConfig`/`cloudConfigContents`.
- `defaultStorageClass` controls which of the chart's two StorageClasses (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`) is the cluster default. The controller always sets **both** `storageClass.delete.isDefault` and `storageClass.retain.isDefault` from this one field (`delete` → `true`/`false`, `retain` → `false`/`true`, `none` → `false`/`false`), so `helmValues` can never create two defaults between this chart's own two classes. It defaults to `delete`. A StorageClass from a *different* provisioner entirely being marked default is a separate, cluster-wide misconfiguration this component does not detect; the runbook documents it as an operator responsibility.
- No `dnsPolicy` override (unlike `CloudControllerManager`): the node plugin's `hostNetwork: true` / `dnsPolicy: ClusterFirstWithHostNet` don't create the same deadlock `CloudControllerManager` has, because a `CsiDriver` reconcile never gates CNI or CoreDNS scheduling the way `CloudControllerManager`'s own Deployment did. The node plugin's `kubeletDir` (`/var/lib/kubelet`) is left at the chart default; Talos's kubelet uses the standard path.
- **One Talos-specific override *is* required, found by live-verifying against a real Talos cluster (not left as a documented open item, unlike the CCM's own overrides which were reasoned out ahead of time):** the chart's default `csi.plugin.volumes` hostPath-mounts `/etc/cacert` (an optional TLS CA bundle) on both the node and controller plugin containers. Talos's root filesystem is read-only and never creates that directory, so the container runtime's `mkdir` for the bind mount fails outright and every `cinder-csi-plugin` container sits in `CreateContainerError` forever. The controller always overrides `csi.plugin.volumes: []` and `csi.plugin.volumeMounts` down to just the required `cloud-config` Secret mount, unconditionally, so a `helmValues` passthrough can never reintroduce the crash.
- No `cleanupTimeoutSeconds`: no operator-owned resources need a bounded wait on removal, same as `CloudControllerManager`.
- Deliberately not typed: `logVerbosityLevel`, `clusterID`, `pvcAnnotations`, individual sidecar images/resources, StorageClass `parameters`. All remain reachable through `helmValues`.

Validation, rejected with `phase: Failed` and a `reason`, like the other three components:

- `metadata.name` does not equal the expected name for `spec.driver` (`openstack-cinder` for `openstackCinder`) — reason `InvalidName`
- `chartVersion` is empty — reason `InvalidChartVersion`
- `chartVersion` has leading or trailing whitespace — reason `InvalidChartVersion`
- `cloudConfigSecretRef.name` is empty or is not a valid Kubernetes object name (DNS-1123 subdomain) — reason `InvalidSecretRef`
- `helmValues` is not a JSON object — reason `InvalidHelmValues`
- `helmValues` sets `secret.data` — reason `InvalidHelmValues`

`platformKind`, `driver` and `defaultStorageClass` are enums with a single or fixed set of values; unsupported values are rejected by the Kubernetes API server against the generated OpenAPI schema before the controller ever sees them, exactly as `CniProvider` and `CloudProvider` are today — no corresponding runtime check is needed.

Status mirrors `CloudControllerManagerStatus` and reuses `Phase`, `Condition` and `AppliedResourceRef`: `phase`, `observedGeneration`, `chartVersion`, `appliedResources`, `conditions`.

### Reconcile

Same shape as the CCM loop:

1. Leader gate, then validate; write `Failed` status on rejection.
2. Render the chart with `helm template` from the classic repo `https://kubernetes.github.io/cloud-provider-openstack`, chart `openstack-cinder-csi`, namespace `kube-system`, `--no-hooks --include-crds`. The existing `--repo` render form is reused; no new render path is expected.
3. Parse, sort and apply the rendered objects in rank order, then prune anything no longer rendered against `status.appliedResources`. The ledger is checkpointed before applying, as in `reconciler-status-and-ledger`. No namespace is synthesized: `kube-system` already exists and Talos's default admission configuration exempts it.
4. Write `Ready` status; requeue at 300s.

Observed by rendering chart `2.36.5` with `secret.enabled=true, secret.create=false, secret.hostMount=false, secret.name=cloud-config`: 2 `ServiceAccount`s, 5 `ClusterRole`/`ClusterRoleBinding` pairs, one node-plugin `DaemonSet` (`hostNetwork: true`, no node selector, tolerates everything — runs on every node including control-plane), one controller-plugin `Deployment` (1 replica, no tolerations), one `CSIDriver` object (`cinder.csi.openstack.org`), and two `StorageClass`es (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`). No `Secret` object is rendered.

### Cleanup

The finalizer deletes applied resources in reverse ledger order. There are no removal waits. Deleting the `CsiDriver` does not delete already-provisioned Cinder volumes, but PVCs or pods still depending on them can be left with a stuck detach/unmount once the plugin is gone. The runbook documents this as an operator responsibility, matching how `CloudControllerManager`'s cleanup section documents that deleting it does not undo node initialization.

### Ordering and the controller-plugin's own scheduling

The node plugin tolerates every taint, so it needs no ordering relative to `CloudControllerManager`. The controller-plugin `Deployment` has no tolerations, so on a cluster whose nodes still carry the `uninitialized` taint it stays `Pending` until `CloudControllerManager` clears it — eventually consistent, not a hard failure, so no toleration is added. The deploy README's documented apply order gains a step: `CloudControllerManager`, then `CniInstallation`, then `CsiDriver`, then `PullThroughCache`.

The controller enforces no ordering between components in code; each reconciles independently once applied.

### Future providers

Each future driver becomes one more `driver` enum value and one more typed per-driver spec field, applied as its own `CsiDriver` CR instance (name matching the driver), reusing the same reconcile loop unchanged:

| Driver enum (proposed) | CR name | Chart / mechanism | Notes |
|---|---|---|---|
| `openstackManila` | `openstack-manila` | `openstack-manila-csi` (same repo as Cinder) | File storage counterpart to Cinder. |
| `awsEbs` | `aws-ebs` | `aws-ebs-csi-driver` | Typically needs IRSA/pod-identity credentials, not a `cloud.conf` Secret — a different credential shape than `cloudConfigSecretRef`. |
| `awsEfs` | `aws-efs` | `aws-efs-csi-driver` | Same credential-shape difference as EBS. |
| `azureDisk` | `azure-disk` | `disk.csi.azure.com` (often a managed AKS add-on) | On self-managed clusters this may need a cloud-config Secret similar to `cloudConfigSecretRef`; on AKS it is out of scope per this spec's "managed platforms" exclusion. |
| `azureFile` | `azure-file` | `file.csi.azure.com` | Same as `azureDisk`. |
| `gcpPersistentDisk` | `gcp-persistent-disk` | `pd.csi.storage.gke.io` | Typically workload-identity based, not a Secret. |
| `gcpFilestore` | `gcp-filestore` | `filestore.csi.storage.gke.io` | Same as persistent disk. |
| `gcpGcsfuse` | `gcp-gcsfuse` | `gcsfuse.csi.storage.gke.io` | Object storage exposed as a CSI driver; same credential shape as the other GCP drivers. |
| `ociBlockVolume` | `oci-block-volume` | `blockvolume.csi.oraclecloud.com` | OCI CSI driver; credential shape not yet researched. |
| `ociFss` | `oci-fss` | `fss.csi.oraclecloud.com` | Same as block volume. |

None of these rows adds Rust code in this slice; they exist so the next provider's design starts from a documented shape instead of a blank page, the same way `CloudControllerManager`'s spec named AWS/GCP/Azure/OCI without building them.

### Code layout

- `src/csi_driver.rs`: CRD types, name validation and the values builder.
- `src/csi_reconciler.rs`: reconcile, cleanup, finalizer wiring, status.
- `src/crd.rs`: `SecretNameRef` moves here from `src/cloud_controller_manager.rs`, since it is now shared by two components; `cloud_controller_manager.rs` re-exports or imports it rather than redefining it.
- `src/main.rs`: fourth controller.
- `src/bin/crdgen.rs`: emits all four CRDs.

### Deployment and docs

- `deploy/crd.yaml` regenerated with all four CRDs.
- `deploy/README.md`: apply order updated to include `CsiDriver`. No RBAC change (the controller is cluster-admin); the RBAC ledger memory gains rows.
- `examples/csi-driver-openstack-cinder.yaml`: a Talos-on-OpenStack starting point, with a comment showing the shape of the `cloud.conf` Secret (reusing the same shape as the `CloudControllerManager` example).
- `docs/runbooks/csi-driver-openstack-cinder-verification.md`: the live checks below.
- A `docs/memory/` entry and index line, per CLAUDE.md.

## Testing

- **Unit:** spec deserialization and defaults; each validation rejection; values builder (typed fields win over `helmValues`, the Secret name and `cloud.conf` filename are passed through, `defaultStorageClass`'s three cases each set both `isDefault` flags correctly, `secret.create` is always `false`).
- **Real-chart (ignored, needs network), in the style of the CCM real-chart test:** render chart `2.36.5` with the controller's values and assert no Secret object, exactly one node `DaemonSet` and one controller `Deployment`, the mount key `cloud.conf`, the `CSIDriver` object, and both StorageClasses with the expected `isDefault` values. Re-run on every chart bump.
- **Example:** the example manifest parses and validates, in the style of `tests/ipv6_example.rs`.
- **Integration (ignored, Talos-in-Docker):** apply the CR, assert the DaemonSet and Deployment appear, delete the CR, assert the applied resources are gone. It does not assert readiness, since the pods cannot run without OpenStack credentials.
- **Live acceptance (manual, real OpenStack, runbook):**
  - a PVC against each StorageClass provisions, attaches, and mounts a Cinder volume; the default StorageClass is honored for a PVC that doesn't name one
  - volume expansion (`allowVolumeExpansion: true` on both StorageClasses)
  - `/var/lib/kubelet` `kubeletDir` behaves correctly on Talos (the `/etc/cacert` mount is no longer a live-verification item: it is unconditionally dropped, see Design)
  - the controller-plugin `Deployment` schedules once `CloudControllerManager` clears the `uninitialized` taint
  - the missing-Secret failure mode (pod stuck `ContainerCreating`, status still `Ready`)
  - delete behavior, as described in Cleanup

## Verification status

Implemented and live-verified on a real Talos-on-OpenStack cluster on 2026-09-28. The `/etc/cacert` hostPath mount, flagged above as a live-verification item, was found broken on first deploy (`CreateContainerError`, "read-only file system") and fixed with the unconditional `csi.plugin.volumes`/`csi.plugin.volumeMounts` override described in Design; after the fix, `status.phase` reaches `Ready`, both StorageClasses render with `csi-cinder-sc-delete` correctly annotated as the cluster default, and the automated integration test (apply → `Ready` → DaemonSet/Deployment exist → delete → cleanup) passes against the live cluster. Not yet exercised: the full PVC provision/attach/mount/expand round trip and the missing-Secret failure mode (both remain runbook items).
