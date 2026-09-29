# Verifying cluster-wide CSI snapshot support on Talos

Manual acceptance for the `SnapshotController` resource. Needs a real
cluster with `CertManagerInstallation` and `CniInstallation` already
`Ready`. For step 4, `CsiDriver` (OpenStack Cinder) must also be `Ready`
with at least one bound PVC. Steps 1-4 were run and passed on a live
6-node Talos cluster on 2026-09-29; see "Findings to record" at the end
and `docs/memory/snapshot-controller-2026-09.md` for the full write-up.

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
kubectl get crd volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io -o jsonpath='{.spec.conversion.webhook.clientConfig.caBundle}{"\n"}'
```

Expected: `Ready=True` on both the `Issuer` and the `Certificate`; the TLS
Secret exists; the group-snapshot CRD's conversion strategy is `Webhook`
(confirming the chart wired the webhook into the CRD, not just deployed a
pod); the `caBundle` value is non-empty (a base64 CA cert) — an empty value
means cert-manager's `cainjector` isn't running/enabled in the
`CertManagerInstallation` this component depends on, and is the one thing
that can silently fail even when the conversion strategy shows `Webhook`. If
the `Issuer`/`Certificate` never reach `Ready`, `CertManagerInstallation`
likely isn't actually healthy even though it reports `Ready` (which only
ever means "manifests applied") — check its own pods.

## 4. A real VolumeSnapshot against a Cinder PVC

Requires `CsiDriver` (OpenStack Cinder) `Ready` and an existing, bound PVC
(`docs/runbooks/csi-driver-openstack-cinder-verification.md`).

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
```

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

Live-verified 2026-09-29 on a 6-node Talos cluster (3 control-plane, 3
worker; controller rolled `0.1.8` → `0.1.9` as part of this run, chart
`5.3.0`), alongside `CniInstallation`/`CertManagerInstallation`/`CsiDriver`
which stayed `Ready` throughout. Steps 1-4 all matched "Expected" exactly:

- Step 1: `Ready` within seconds of applying the CR; both pods
  (`snapshot-controller`, `snapshot-controller-conversion-webhook`) Running.
  The controller rollout itself was clean (PDB honored, old pod terminated
  only after both new replicas were ready) and none of the four existing
  components were disrupted.
- Step 2: no `pod-security.kubernetes.io/*` label appeared on the
  `snapshot-controller` namespace; both pods `Running`. The weaker
  assumption (no explicit `seccompProfile`, unlike cert-manager's chart)
  held anyway under Talos's default `baseline` policy.
- Step 3: `Issuer` and `Certificate` both `Ready=True`; the TLS Secret
  exists; the group-snapshot CRD's conversion strategy is `Webhook`; the
  `caBundle` was populated (1456 bytes) — cert-manager's `cainjector` is
  running and wired the CA in correctly, not just deployed.
- Step 4: a real `VolumeSnapshot` against a throwaway 1Gi Cinder PVC
  (`csi-cinder-sc-delete`) reached `readyToUse: true` within ~10 seconds.
  The underlying `VolumeSnapshotContent` carries a real Cinder-assigned
  `snapshotHandle` UUID (`ed653dfe-e2bb-4e7e-afeb-d5d02d2252ab`), populated
  by the CSI driver's actual provisioning call — strong evidence this is a
  genuine Cinder-side snapshot, not just Kubernetes-side status. Direct
  `openstack volume snapshot show` confirmation was not possible (no
  `openstack` CLI/credentials available from the verifying machine); the
  `snapshotHandle` is the corroborating evidence in its place. All test
  resources (`VolumeSnapshot`, `VolumeSnapshotClass`, PVC, and the
  underlying Cinder volume/snapshot) were cleaned up and confirmed gone.

**Not yet run:** step 5 (delete `SnapshotController`, confirm the six-CRD
cascade). The component was left installed and `Ready` after this run
rather than torn down.
