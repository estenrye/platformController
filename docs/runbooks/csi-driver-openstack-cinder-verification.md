# Verifying the OpenStack Cinder CSI driver on Talos

Manual acceptance for the `CsiDriver` resource (`driver: openstackCinder`).
Needs a real OpenStack cloud you can boot Talos VMs in, credentials for it, a
`CloudControllerManager` already `Ready` (see
`docs/runbooks/cloud-controller-manager-verification.md`), and a `CniInstallation`
already `Ready`. Nothing here has been run yet: record what you observe under
"Findings to record" at the end.

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

**Expected exception:** the controller plugin's `csi-snapshotter` container
specifically will be `CrashLoopBackOff` here and stay that way, showing the
overall pod `0/1 READY` even once everything else is healthy. It calls
`ensureCustomResourceDefinitionsExist` at startup and exits non-zero unless the
cluster already has the external-snapshotter CRDs (`snapshot.storage.k8s.io`)
and `snapshot-controller` installed -- a separate, cluster-level concern this
component's spec deliberately excludes (see the spec's Non-goals). Block-volume
provisioning, attach, mount and expansion (via the provisioner, attacher and
resizer sidecars) are unaffected. To silence the crash-loop instead of
installing snapshot support, set `spec.openstackCinder.helmValues: {csi:
{snapshotter: {enabled: false}}}` on the `CsiDriver` (this flag exists in the
chart's real values).

## 3. StorageClasses and the default

```sh
kubectl get storageclass
```

Expected: `csi-cinder-sc-delete` and `csi-cinder-sc-retain`, both provisioner
`cinder.csi.openstack.org`. With the example's default `defaultStorageClass:
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

To fill in from the first live run: where the pods scheduled and how long
after both prerequisites were `Ready` (step 1-2), the provision/attach/mount/
expand round trip (step 4), the `/etc/cacert` and `kubeletDir` mount behavior
on Talos (step 5), and anything in step 7 that differs from "Expected".
