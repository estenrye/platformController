# Verifying the OpenStack Cinder CSI driver on Talos

Manual acceptance for the `CsiDriver` resource (`driver: openstackCinder`).
Needs a real OpenStack cloud you can boot Talos VMs in, credentials for it, a
`CloudControllerManager` already `Ready` (see
`docs/runbooks/cloud-controller-manager-verification.md`), and a `CniInstallation`
already `Ready`. First run: 2026-09-28, against a real Talos-on-OpenStack lab
cluster; see "Findings to record" at the end for what was and wasn't verified.

## 1. Apply order

```sh
kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl get cni default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl get nodes -o custom-columns=NAME:.metadata.name,TAINTS:.spec.taints[*].key
```

Expected: no node still carries `node.cloudprovider.kubernetes.io/uninitialized`.
The controller-plugin Deployment this CR creates has no toleration for that
taint and runs on the regular pod network, so both prerequisites above must
already be `Ready` before it can schedule.

## 2. Create the credentials Secret, then apply

Reuse the same `cloud-config` Secret the CCM runbook created, or create a
separate one if this cluster uses different credentials for storage:

```sh
kubectl apply -f examples/csi-driver-openstack-cinder.yaml
kubectl get csi openstack-cinder -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n kube-system get ds openstack-cinder-csi-nodeplugin -o wide
kubectl -n kube-system get deploy openstack-cinder-csi-controllerplugin -o wide
kubectl -n kube-system get pods -l app=openstack-cinder-csi -o wide
```

Expected: `Ready`; the node plugin DaemonSet has one pod per node, Running; the
controller plugin Deployment has one pod, Running. `Ready` means the manifests
were applied, not that the driver is healthy: the pods are the real signal.
Check `kubectl -n kube-system logs <pod>` for errors reaching Keystone or Cinder.

**Expected noise, live-verified 2026-09-28 (corrects an earlier, wrong claim in
this runbook that the sidecar crash-loops -- it doesn't):** the controller
plugin's `csi-snapshotter` container (chart `2.36.5`, sidecar `v8.4.0`) logs a
continuous stream of `Failed to watch ... the server could not find the
requested resource (get volumesnapshotclasses.snapshot.storage.k8s.io /
volumesnapshotcontents.snapshot.storage.k8s.io)` errors, retrying with backoff,
for as long as the cluster lacks the external-snapshotter CRDs
(`snapshot.storage.k8s.io`) and `snapshot-controller` -- a separate,
cluster-level concern this component's spec deliberately excludes (see the
spec's Non-goals). The container itself stays `Running`, the pod reaches
`6/6 READY`, and block-volume provisioning, attach, mount and expansion are
unaffected; only volume snapshots don't work until that cluster-level install
happens. To silence the log noise instead of installing snapshot support, set
`spec.openstackCinder.helmValues: {csi: {snapshotter: {enabled: false}}}` on
the `CsiDriver` (this flag exists in the chart's real values).

## 3. StorageClasses and the default

```sh
kubectl get storageclass
```

Expected: `csi-cinder-sc-delete` and `csi-cinder-sc-retain`, both provisioner
`cinder.csi.openstack.org`. With the example's default `storageClasses.default:
delete`, `csi-cinder-sc-delete` is annotated
`storageclass.kubernetes.io/is-default-class: "true"` and is the one a PVC that
names no `storageClassName` gets.

## 4. Provision, attach, mount, expand

```sh
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: csi-cinder-test
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 1Gi
EOF
kubectl get pvc csi-cinder-test -w        # Bound
kubectl run csi-cinder-test-pod --image=registry.k8s.io/pause:3.10 \
  --overrides='{"spec":{"containers":[{"name":"pause","image":"registry.k8s.io/pause:3.10","volumeMounts":[{"name":"data","mountPath":"/data"}]}],"volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"csi-cinder-test"}}]}}'
kubectl get pod csi-cinder-test-pod -w    # Running
```

Expected: the PVC binds, a Cinder volume appears (`openstack volume list`), and
the pod mounts it. Then expand it:

```sh
kubectl patch pvc csi-cinder-test -p '{"spec":{"resources":{"requests":{"storage":"2Gi"}}}}'
kubectl get pvc csi-cinder-test -o jsonpath='{.status.capacity.storage}{"\n"}'
```

Expected: capacity grows to `2Gi` (`allowVolumeExpansion: true` on both
StorageClasses). Delete the pod and PVC afterwards and confirm the Cinder
volume is removed (`openstack volume list`) for `csi-cinder-sc-delete`, and
confirm it is *not* removed for a PVC against `csi-cinder-sc-retain`.

**Known caveat, live-verified 2026-09-28: Nova/Cinder availability-zone
mismatch is possible and is not something this component can fix.** The
chart runs the provisioner with `--with-topology=true`, so by default it
derives a new volume's `availability` from the scheduling node's
`topology.kubernetes.io/zone` label -- which cloud-provider-openstack sets
from the node's **Nova** compute AZ. On a cluster where Cinder's own AZ list
(`GET /os-availability-zone` on the Cinder endpoint) doesn't include that
same name -- a legitimate, common OpenStack deployment shape, since Nova and
Cinder AZs are independently configured and many deployments leave Cinder's
at its default `nova` -- the plain `csi-cinder-sc-delete`/`-retain`
StorageClasses fail provisioning outright:
`CreateVolume failed ... Availability zone '<nova-az>' is invalid`. This was
hit on first live run here (Nova AZ `pcd-ce-lab`, Cinder AZ only `nova`).
Provisioning and deletion were confirmed working end-to-end against real
Cinder using a one-off StorageClass with `parameters: {availability: nova}`
(bypassing topology-derived AZ selection); but the resulting PV's node
affinity then required a node labeled `zone: nova`, which none of this
cluster's nodes are (they're all `pcd-ce-lab`), so a pod could not schedule
against that volume in this lab. If your Nova and Cinder AZs diverge, the
fix is either an explicit `parameters.availability` matching a real Cinder
AZ *and* matching node topology, or disabling topology entirely via
`spec.openstackCinder.helmValues: {csi: {provisioner: {topology: "false"}}}`
on the `CsiDriver`. Not something the typed API covers (AZ/topology is a
documented Non-goal); this is an operator-facing StorageClass-design
decision, not a controller bug.

## 5. Node plugin mounts on Talos

The `/etc/cacert` hostPath is now unconditionally dropped by `build_values`
(live-verified: it crashed every `cinder-csi-plugin` container with
`CreateContainerError: read-only file system` before this override existed).
Confirm it stays gone and that `/var/lib/kubelet` `kubeletDir` still mounts:

```sh
kubectl -n kube-system get pod -l component=nodeplugin -o name | head -1 | \
  xargs -I{} kubectl -n kube-system exec {} -c cinder-csi-plugin -- mount | grep -c cacert   # 0
kubectl -n kube-system get pod -l component=nodeplugin -o name | head -1 | \
  xargs -I{} kubectl -n kube-system exec {} -c cinder-csi-plugin -- mount | grep kubelet
```

Expected: `0` cacert mounts; the kubelet directory mount present and healthy.

## 6. Missing Secret

**Warning:** if step 2 reused the CCM's `cloud-config` Secret rather than
creating a separate one, deleting it here also breaks the CCM once its own
pods restart against it. Recreate the Secret immediately after this check and
confirm the CCM's pods recover too (not just the CSI driver's), or avoid this
entirely by using a separate Secret name for the CSI driver from the start.

```sh
kubectl -n kube-system delete secret cloud-config
kubectl -n kube-system delete pod -l app=openstack-cinder-csi
kubectl -n kube-system get pods -l app=openstack-cinder-csi      # ContainerCreating
kubectl get csi openstack-cinder -o jsonpath='{.status.phase}{"\n"}'   # still Ready
```

Expected (and by design): the pods cannot mount the Secret and stay
`ContainerCreating`, while status still says `Ready`. Recreate the Secret and
the pods start -- check the CCM's pods too if it shares this Secret.

## 7. Delete, and what stays

```sh
kubectl delete csi openstack-cinder        # returns once the finalizer clears
kubectl -n kube-system get ds openstack-cinder-csi-nodeplugin          # NotFound
kubectl -n kube-system get deploy openstack-cinder-csi-controllerplugin # NotFound
openstack volume list
```

Expected: the chart's objects are gone. Any Cinder volume from a
`csi-cinder-sc-retain` PVC still exists; deleting the `CsiDriver` does not
delete provisioned volumes, and a pod still mounting one at delete time can be
left with a stuck detach/unmount.

## When something goes wrong

- `kubectl get csi openstack-cinder -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidSecretRef`,
  `InvalidHelmValues`, `Unsupported` -- including a `metadata.name` that does not
  match the driver's expected name; `RenderFailed` when helm cannot render the
  chart, e.g. the app version `v1.36.0` used as the chart version;
  `InvalidManifest`; `ApplyFailed` when the API server rejects an object) and
  the error text.
- The ledger (`status.appliedResources`) is saved before anything is applied,
  so deleting the resource after a failed first install still removes
  everything that was created.
- An `ApplyFailed` on the `CSIDriver` object or either `StorageClass`
  (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`) after a chart version bump
  likely means a field that's immutable on update changed (`attachRequired`,
  `podInfoOnMount` or `volumeLifecycleModes` on the `CSIDriver`;
  `provisioner` or `reclaimPolicy` on a `StorageClass`), and every reconcile
  will keep failing the same way. Recover by deleting that specific object by
  hand (`kubectl delete csidrivers.storage.k8s.io cinder.csi.openstack.org` --
  the fully-qualified built-in resource, not this operator's own
  `csidrivers.platform.rye.ninja` CR -- or `kubectl delete storageclass
  csi-cinder-sc-delete`/`csi-cinder-sc-retain` as appropriate) and letting the
  next reconcile recreate it.

## Findings to record

From the first live run, 2026-09-28, against a real Talos-on-OpenStack lab
cluster (already running `CloudControllerManager` and `CniInstallation`,
both `Ready`, no lingering `uninitialized` taints):

- **Step 1-2:** both plugins scheduled immediately once the CR was applied
  (controller-plugin and every node-plugin pod `Running` within ~30s);
  `status.phase` reached `Ready`. Confirmed the fixed image (see the
  `/etc/cacert` finding below) is required for this -- before the fix, every
  `cinder-csi-plugin` container sat in `CreateContainerError` instead.
- **Step 2's `csi-snapshotter` note:** the sidecar does *not* crash-loop
  (an earlier version of this runbook wrongly said it did, based on
  inference rather than a live run). It logs continuous, harmless
  `Failed to watch ...` retries for `VolumeSnapshotClass`/
  `VolumeSnapshotContent` and stays `Running`/`6/6 READY`.
- **Step 3:** both StorageClasses rendered correctly; `csi-cinder-sc-delete`
  carried the `is-default-class: "true"` annotation, `csi-cinder-sc-retain`
  did not.
- **Step 4 (provision/attach/mount/expand):** provisioning against the
  plain StorageClasses failed with `Availability zone '<nova-az>' is
  invalid` -- this cluster's Cinder only has AZ `nova`, while Nova's compute
  AZ (and the node topology label the topology-aware provisioner defaults
  to) is a different name. See the caveat under step 4 for the full
  explanation. Provisioning and deletion were confirmed working end-to-end
  against real Cinder (verified directly against the Cinder API, not just
  Kubernetes-side status) using a StorageClass with an explicit
  `parameters.availability: nova` override; the resulting PV's node
  affinity then had no matching node in this lab, so pod attach/mount and
  volume expansion were not completed here. Both are still open items for a
  cluster whose Nova and Cinder AZs coincide (or with topology disabled).
- **Step 5:** the `/etc/cacert` hostPath mount broke every `cinder-csi-plugin`
  container on Talos (`CreateContainerError: read-only file system`) before
  the fix in this same session; after the fix, `mount | grep -c cacert`
  correctly returns `0` and the `kubeletDir` mount is present and healthy.
- **Step 6-7:** not yet run in this session.
