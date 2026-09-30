# Verifying cluster-wide CSI snapshot support on Talos

Manual acceptance for the `SnapshotController` resource. Needs a real
cluster with `CniInstallation` already `Ready`. For step 3, `CsiDriver`
(OpenStack Cinder) must also be `Ready` with at least one bound PVC. Steps
1-3 and 4 (renumbered below) were run and passed on a live 6-node Talos
cluster on 2026-09-29; see "Findings to record" at the end and
`docs/memory/snapshot-controller-2026-09.md` for the full write-up,
including why `groupSnapshotsEnabled` defaults to `false`.

## 1. Apply and reach Ready

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/snapshotcontrollers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/snapshot-controller.yaml
kubectl get snapctl default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n snapshot-controller get pods -o wide
```

Expected: `Ready`; one pod (`snapshot-controller`), Running. No
`CertManagerInstallation` prerequisite with the example's default
`groupSnapshotsEnabled: false`.

## 2. Namespace admission under the default (`baseline`) Pod Security Standard

The design assumes this chart's pods need no `pod-security.kubernetes.io/*`
labels on their namespace, based on rendering the chart (no hostPath, no
hostNetwork), but — unlike cert-manager's chart — it sets no explicit
`seccompProfile`, so this is a weaker assumption than cert-manager's. Confirm
the pod actually started with no admission rejection:

```sh
kubectl get namespace snapshot-controller -o jsonpath='{.metadata.labels}{"\n"}'   # no pod-security labels
kubectl -n snapshot-controller get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
```

Expected: no `pod-security.kubernetes.io/*` label on the namespace; the pod
`Running`. If it's stuck `Pending` with a Pod Security admission error,
`snapshot_controller_namespace_object` in
`src/snapshot_controller_reconciler.rs` needs the same `privileged` labels
Calico's and Spegel's namespaces carry, and this runbook and the design spec
both need updating to say so.

## 3. A real VolumeSnapshot against a Cinder PVC

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

## 4. Group snapshots (opt-in, currently non-functional on OpenStack)

`spec.groupSnapshotsEnabled: true` installs the CRD conversion webhook and a
self-signed `cert-manager.io` `Issuer` for its TLS — this then requires
`CertManagerInstallation` to be `Ready` first. This is documented here as a
**known-broken path**, not a step expected to pass:

```sh
kubectl -n snapshot-controller get issuer snapshot-controller-selfsigned -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl -n snapshot-controller get certificate snapshot-controller-conversion-webhook -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl get crd volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io -o jsonpath='{.spec.conversion.webhook.clientConfig.caBundle}{"\n"}'
```

The `Issuer`/`Certificate`/webhook infrastructure itself comes up correctly
(live-verified 2026-09-29: both `Ready=True`, `caBundle` populated by
cert-manager's `cainjector`). But creating an actual `VolumeGroupSnapshot`
fails on every attempt:

```sh
kubectl apply -f - <<'EOF'
apiVersion: groupsnapshot.storage.k8s.io/v1
kind: VolumeGroupSnapshotClass
metadata:
  name: csi-cinder-groupsnapclass
driver: cinder.csi.openstack.org
deletionPolicy: Delete
EOF
kubectl apply -f - <<'EOF'
apiVersion: groupsnapshot.storage.k8s.io/v1
kind: VolumeGroupSnapshot
metadata:
  name: groupsnap-smoketest
  namespace: default
spec:
  volumeGroupSnapshotClassName: csi-cinder-groupsnapclass
  source:
    selector:
      matchLabels: { smoketest: groupsnap }   # label an existing PVC with this first
EOF
kubectl get events -n default --field-selector involvedObject.name=groupsnap-smoketest
```

Expected (and live-confirmed 2026-09-29): `readyToUse` never becomes `true`;
events show `unexpected conversion version from
"groupsnapshot.storage.k8s.io/v1" to "groupsnapshot.storage.k8s.io/v1beta2"`,
retried forever. Separately, `cinder-csi-plugin`'s own startup log
(`kubectl -n kube-system logs <controllerplugin pod> -c cinder-csi-plugin |
grep capability`) never lists `CREATE_DELETE_GROUP_SNAPSHOT` — even a working
webhook wouldn't help, since the driver has no code path to fulfill the
request. Clean up:

```sh
kubectl delete volumegroupsnapshot groupsnap-smoketest
kubectl delete volumegroupsnapshotclass csi-cinder-groupsnapclass
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
deleted at the end of step 3.

## Migrating an existing cluster from `groupSnapshotsEnabled: true` to `false`

Live-hit on 2026-09-29: if `SnapshotController` was ever `Ready` on this
cluster with the webhook enabled (either explicitly, or because you deployed
before this toggle existed, when it was unconditionally on), upgrading to a
controller version with `groupSnapshotsEnabled: false` gets stuck in a
permanent `Failed`/`ApplyFailed` loop:

```
failed to apply CustomResourceDefinition/volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io:
ApiError: [...] spec.conversion.strategy: Required value, spec.conversion.webhookClientConfig:
Forbidden: should not be set when strategy is not set to Webhook
```

**Root cause:** the CRD carries the annotation
`cert-manager.io/inject-ca-from: snapshot-controller/snapshot-controller-conversion-webhook`,
which tells cert-manager's `cainjector` to continuously maintain
`spec.conversion.webhook.clientConfig.caBundle` on this object using its
*own* field manager, independent of this controller's server-side apply. The
new render omits `spec.conversion` entirely, so this controller's own apply
releases the fields *it* owns (`strategy`, `webhook.clientConfig.service`) —
but `cainjector`'s separately-owned `caBundle` field survives the same
apply, leaving the object in a state the API server rejects outright
(`webhookClientConfig` present without `strategy: Webhook`). `cainjector`
would eventually notice the annotation is gone and clean up after itself,
but it can't, because this controller's apply — the only thing that would
remove the annotation — never succeeds in the first place. A one-time manual
break of that cycle is required:

```sh
kubectl annotate crd volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io cert-manager.io/inject-ca-from-
kubectl patch crd volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io --type=json -p='[{"op":"remove","path":"/spec/conversion"}]'
```

Both commands act only on the CRD's own metadata/spec, never on any
`VolumeGroupSnapshotContent` instance — confirm none exist first
(`kubectl get volumegroupsnapshotcontents.groupsnapshot.storage.k8s.io -A`)
so there's no conversion-strategy risk to real data; this cluster had none.
After this, the controller's next automatic retry (`error_policy`'s 30s
backoff) reaches `Ready` and the stale conversion-webhook
Deployment/Service/Certificate get pruned normally.
Cert-manager's own `Certificate` deletion does **not** delete the TLS Secret
it created (standard cert-manager behavior, to avoid losing certs), so that
Secret is left orphaned in the `snapshot-controller` namespace — harmless,
but worth a manual `kubectl -n snapshot-controller delete secret
snapshot-controller-conversion-webhook` for a fully tidy state.

## When something goes wrong

- `kubectl get snapctl default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidHelmValues`,
  `Unsupported`; `RenderFailed` when helm cannot render the chart;
  `InvalidManifest`; `ApplyFailed` — including both the case where
  `groupSnapshotsEnabled: true` and `CertManagerInstallation` isn't `Ready`
  yet, and the migration case above) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is
  applied, so deleting the resource after a failed first install still
  removes everything that was created.

## Findings to record

Live-verified 2026-09-29 on a 6-node Talos cluster (3 control-plane, 3
worker; controller rolled `0.1.8` → `0.1.9` → `0.1.10`, chart `5.3.0`),
alongside `CniInstallation`/`CertManagerInstallation`/`CsiDriver` which
stayed `Ready` throughout every rollout:

- Steps 1-2, re-run against `0.1.10` with the corrected `groupSnapshotsEnabled:
  false` default: `Ready`; exactly one pod (`snapshot-controller`), no
  webhook Deployment/Service, matching the design. Namespace admission held
  with no `pod-security.kubernetes.io/*` label. Both controller rollouts
  (`0.1.9`→`0.1.10` for this correction) were clean, PDB honored, no
  disruption to the other four components.
- **The migration case above was hit for real, not just reasoned about**:
  this cluster had already run `groupSnapshotsEnabled: true`-equivalent
  behavior (from before the toggle existed), so rolling to `0.1.10` produced
  the exact `Failed`/`ApplyFailed` loop the migration section describes.
  The documented two-command remediation
  (`kubectl annotate ... cert-manager.io/inject-ca-from-` +
  `kubectl patch ... --type=json` removing `/spec/conversion`) resolved it
  on the first attempt; the controller reached `Ready` on its next
  automatic retry with no further intervention. No `VolumeGroupSnapshot*`
  instances existed at the time, confirmed before touching the CRD, so
  there was no data-loss risk. The orphaned TLS Secret
  (`snapshot-controller-conversion-webhook`) left behind by the pruned
  `Certificate` was also confirmed and manually deleted.
- Step 3 (`VolumeSnapshot`): a real `VolumeSnapshot` against a throwaway 1Gi
  Cinder PVC (`csi-cinder-sc-delete`) reached `readyToUse: true` within ~10
  seconds. The underlying `VolumeSnapshotContent` carries a real
  Cinder-assigned `snapshotHandle` UUID
  (`ed653dfe-e2bb-4e7e-afeb-d5d02d2252ab`), populated by the CSI driver's
  actual provisioning call. Direct `openstack volume snapshot show`
  confirmation was not possible (no `openstack` CLI/credentials available
  from the verifying machine); the `snapshotHandle` is the corroborating
  evidence in its place. All test resources were cleaned up and confirmed
  gone.
- Step 4 (group snapshots, `groupSnapshotsEnabled: true`): this is what
  prompted the toggle. `Issuer`/`Certificate` both reached `Ready=True`,
  `caBundle` was populated (1456 bytes) — the webhook infrastructure itself
  is genuinely wired up correctly. But every `VolumeGroupSnapshot` attempt
  failed with `unexpected conversion version from
  "groupsnapshot.storage.k8s.io/v1" to "groupsnapshot.storage.k8s.io/v1beta2"`,
  retried forever (event reason `GroupSnapshotContentCreationFailed`).
  Separately, `cinder-csi-plugin`'s startup log lists only
  `CREATE_DELETE_SNAPSHOT` among controller capabilities — no
  `CREATE_DELETE_GROUP_SNAPSHOT` — confirmed absent from
  `kubernetes/cloud-provider-openstack`'s entire source via a code search,
  not just this one deployed version. All test resources were cleaned up
  and confirmed gone.

**Not yet run:** step 5 (delete `SnapshotController`, confirm the six-CRD
cascade). The component was left installed and `Ready` after this run
rather than torn down.
