# Verifying cluster-wide CSI snapshot support on Talos

Manual acceptance for the `SnapshotController` resource. Needs a real
cluster with `CertManagerInstallation` and `CniInstallation` already
`Ready`. For step 4, `CsiDriver` (OpenStack Cinder) must also be `Ready`
with at least one bound PVC. Nothing here has been run yet: record what you
observe under "Findings to record" at the end.

## 1. Apply and reach Ready

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/snapshotcontrollers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cert-manager.yaml
kubectl wait --for=jsonpath='{.status.phase}'=Ready certmgr/default --timeout=300s
kubectl apply -f examples/snapshot-controller.yaml
kubectl get snapctl default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n snapshot-controller get pods -o wide
```

Expected: `Ready`; two pods (`snapshot-controller`,
`snapshot-controller-conversion-webhook`), both Running.

## 2. Namespace admission under the default (`baseline`) Pod Security Standard

The design assumes this chart's pods need no `pod-security.kubernetes.io/*`
labels on their namespace, based on rendering the chart (no hostPath, no
hostNetwork), but — unlike cert-manager's chart — it sets no explicit
`seccompProfile`, so this is a weaker assumption than cert-manager's. Confirm
the pods actually started with no admission rejection:

```sh
kubectl get namespace snapshot-controller -o jsonpath='{.metadata.labels}{"\n"}'   # no pod-security labels
kubectl -n snapshot-controller get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
```

Expected: no `pod-security.kubernetes.io/*` label on the namespace; both
pods `Running`. If either is stuck `Pending` with a Pod Security admission
error, `snapshot_controller_namespace_object` in
`src/snapshot_controller_reconciler.rs` needs the same `privileged` labels
Calico's and Spegel's namespaces carry, and this runbook and the design spec
both need updating to say so.

## 3. The self-signed Issuer and the webhook's Certificate

```sh
kubectl -n snapshot-controller get issuer snapshot-controller-selfsigned -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl -n snapshot-controller get certificate snapshot-controller-conversion-webhook -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl -n snapshot-controller get secret snapshot-controller-conversion-webhook
kubectl get crd volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io -o jsonpath='{.spec.conversion.strategy}{"\n"}'   # Webhook
```

Expected: `Ready=True` on both the `Issuer` and the `Certificate`; the TLS
Secret exists; the group-snapshot CRD's conversion strategy is `Webhook`
(confirming the chart wired the webhook into the CRD, not just deployed a
pod). If the `Issuer`/`Certificate` never reach `Ready`, `CertManagerInstallation`
likely isn't actually healthy even though it reports `Ready` (which only
ever means "manifests applied") — check its own pods.

## 4. A real VolumeSnapshot against a Cinder PVC

Requires `CsiDriver` (OpenStack Cinder) `Ready` and an existing, bound PVC
(`docs/runbooks/csi-driver-openstack-cinder-verification.md`).

```sh
kubectl apply -f - <<'EOF'
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: smoketest-snapshot
  namespace: default
spec:
  volumeSnapshotClassName: csi-cinder-snapclass
  source:
    persistentVolumeClaimName: <an existing bound PVC's name>
EOF
```

`csi-cinder-snapclass` does not exist yet — this component does not create
any `VolumeSnapshotClass` (typed `VolumeSnapshotClass` support on `CsiDriver`
is out of scope, see the design spec's Non-goals). Create one manually first:

```sh
kubectl apply -f - <<'EOF'
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshotClass
metadata:
  name: csi-cinder-snapclass
driver: cinder.csi.openstack.org
deletionPolicy: Delete
EOF
kubectl get volumesnapshot smoketest-snapshot -o jsonpath='{.status.readyToUse}{"\n"}'
```

Expected: `true` within a couple of minutes. Confirm the underlying Cinder
snapshot exists via the Cinder API directly (`openstack volume snapshot
list`), the same live-verification standard the Cinder PVC provisioning
claim already met — not just Kubernetes-side status. Clean up:

```sh
kubectl delete volumesnapshot smoketest-snapshot
kubectl delete volumesnapshotclass csi-cinder-snapclass
```

## 5. Delete, and what stays

```sh
kubectl delete snapctl default        # returns once the finalizer clears
kubectl get namespace snapshot-controller    # NotFound
kubectl get crd volumesnapshots.snapshot.storage.k8s.io   # NotFound
```

Expected: the chart's objects, **including its six CRDs**, are gone.
Kubernetes deletes every instance of a kind when its CRD is deleted, so this
destroys **every** `VolumeSnapshot`/`VolumeSnapshotContent`/
`VolumeSnapshotClass`/`VolumeGroupSnapshot*` in the cluster, not just the
smoke test's own — confirm the smoke test's own resources were already
deleted at the end of step 4.

## When something goes wrong

- `kubectl get snapctl default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidHelmValues`,
  `Unsupported`; `RenderFailed` when helm cannot render the chart;
  `InvalidManifest`; `ApplyFailed` — including the case where
  `CertManagerInstallation` isn't `Ready` yet, since the `Issuer`'s
  `cert-manager.io/v1` kind isn't registered) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is
  applied, so deleting the resource after a failed first install still
  removes everything that was created.

## Findings to record

To fill in from the first live run: whether the namespace-admission
assumption in step 2 held, the Issuer/Certificate/conversion-strategy result
in step 3, the VolumeSnapshot result in step 4 (Kubernetes-side and Cinder
API-side), and anything in step 5 that differs from "Expected".
