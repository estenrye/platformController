# SnapshotController (CSI Volume Snapshot Support)

Status: Draft, awaiting review
Date: 2026-09-29

## Purpose

`CsiDriver`'s own spec (Non-goals) already flags the gap: the OpenStack Cinder
chart's `csi-snapshotter` sidecar renders and runs by default, but nothing in
this controller installs the cluster-wide `snapshot.storage.k8s.io` CRDs or
the `snapshot-controller` Deployment those sidecars need to actually produce a
`VolumeSnapshot`. Without them the sidecar just logs continuous, harmless
`could not find the requested resource` retries (live-verified,
[[csi-driver-openstack-cinder-2026-09]]) and volume snapshotting silently
doesn't work.

This spec adds a sixth platform component, `SnapshotController`, that installs
that missing cluster-level piece: the CRDs, the controller, and — because
group-snapshot support and its conversion webhook are in scope for this slice
— a self-signed `cert-manager` `Issuer` and `Certificate` for the webhook's
TLS. It resumes the queued sub-project B recorded in
[[csi-snapshot-support-2026-09]] (sub-project A, typed `StorageClass`
customization on `CsiDriver`, already shipped), but departs from that
project's research in one material way: that research assumed
`kubernetes-csi/external-snapshotter` had no Helm chart and would need a new
raw-YAML render path. It does have one — `piraeusdatastore/helm-charts`'
`snapshot-controller` chart, sourced directly from the same upstream project —
so this design reuses the existing `helm.rs` render pipeline unchanged, the
same way every other component does.

This component depends on `CertManagerInstallation` (already shipped,
[[cert-manager-2026-09]]) being applied first, for the `Issuer`/`Certificate`
CRDs the webhook's TLS setup needs. `CertManagerInstallation` itself
deliberately installs no `Issuer`/`ClusterIssuer` — this is the "future,
separate component" its own Non-goals pointed at.

## Non-goals

- **Typed `VolumeSnapshotClass` objects on `CsiDriver`.** The original request
  bundled this in; it was explicitly descoped during brainstorming to keep
  this slice to the cluster-level CRDs/controller/webhook. In the meantime,
  `helmValues.volumeSnapshotClasses`/`volumeGroupSnapshotClasses` on this
  component's own chart — or `helmValues` on `CsiDriver` — reach the same
  result untyped. A future slice can give it the same typed shape
  `storageClasses.additional[]` gave `StorageClass` customization.
- **Auto-detecting which CRD/controller version matches a running
  `csi-snapshotter` sidecar's image.** The VolumeSnapshot `v1` API has been
  stable since Kubernetes 1.20; `chartVersion` is pinned explicitly, the same
  convention as every other component.
- **A health-derived `Ready` condition.** `Ready` means "manifests applied",
  same as everywhere else — not "the webhook is serving" or "a test
  `VolumeSnapshot` was issued".
- **Waiting for `CertManagerInstallation` inside this spec's own validation.**
  This component's reconcile waits for the `Issuer` kind to become available
  (see Reconcile) and surfaces that as a retried `Failed` status if
  `CertManagerInstallation` isn't there yet; it does not check for
  `CertManagerInstallation`'s existence as a CR, the same "no cross-CR
  ownership checks" stance `CsiDriver` already takes toward
  `CloudControllerManager`'s taint-clearing.
- **An airgapped chart source.** Fetched from `https://piraeus.io/helm-charts/`,
  so it needs that egress, same caveat as every other chart-fetching
  component.
- **Refactoring the six reconcilers into a shared component framework.**
  Flagged as a growing case since the third CRD, still out of scope here.

## Design

### API

A new cluster-scoped singleton CRD, flat like `CertManagerInstallation` (no
alternative implementation to model, so no provider enum):

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: SnapshotController        # cluster-scoped, shortname "snapctl"
metadata:
  name: default                 # singleton, same rule as the other five
spec:
  platformKind: talos-linux     # only value accepted today
  chartVersion: "5.3.0"         # required, no default, same as every other chart
  helmValues: {}                # optional free-form passthrough
```

- `chartVersion` is the `snapshot-controller` chart's own version (chart and
  app versions do not track together on this chart — `5.3.0` renders app
  `v8.6.0` — so this needs live confirmation and a note in the example, the
  same caution the Cinder spec already applies to chart-vs-app version
  drift).
- `helmValues` is merged first; the controller's own typed values are
  overlaid afterwards, so a passthrough can never disable or contradict them.
  Forced, not user-controlled: `installCRDs: true`, `webhook.enabled: true`,
  `webhook.tls.autogenerate: false`, `webhook.tls.certManagerIssuerRef: {name:
  snapshot-controller-selfsigned, kind: Issuer}`. Rejected as
  `InvalidHelmValues` if `helmValues` sets any of `installCRDs`,
  `webhook.enabled`, or `webhook.tls.*` — the same "don't silently let a
  passthrough reintroduce a broken state" rule `CsiDriver` applies to
  `secret.data`, made an explicit rejection here rather than a silent
  overlay-and-ignore, since a webhook TLS misconfiguration fails loudly
  (`Failed` cert issuance) rather than quietly like a re-added hostPath mount
  would.
- No `cleanupTimeoutSeconds`: no DaemonSet-shaped, node-resident resources
  need a bounded wait on removal, same as `CertManagerInstallation`.

Validation, rejected with `phase: Failed` and a `reason`, same pattern as
every other component:

- the name is not `default` — `Unsupported`
- `platformKind` is unsupported — `Unsupported`
- `chartVersion` is empty, or has leading/trailing whitespace —
  `InvalidChartVersion`
- `helmValues` is not a JSON object, or sets `installCRDs` /
  `webhook.enabled` / any `webhook.tls.*` key — `InvalidHelmValues`

Status mirrors every other kind and reuses `Phase`, `Condition` and
`AppliedResourceRef`: `phase`, `observedGeneration`, `chartVersion`,
`appliedResources`, `conditions`.

### Reconcile

1. Leader gate, then validate; write `Failed` status on rejection.
2. **Wait for the `Issuer` kind (`cert-manager.io/v1`) to be available**,
   via the existing `apply::wait_for_kind_available` — the same utility
   `CniInstallation` already uses to wait for the tigera operator's own CRDs.
   This is a hard dependency: applying an `Issuer` object before its CRD
   exists is a discovery failure, not an eventually-consistent Pending pod
   like `CsiDriver`'s ordering against `CloudControllerManager`. Timing out
   here writes `Failed`/`ApplyFailed` (the same reason
   `ApplyError::KindNotAvailable` already maps to, no new variant or reason
   string) and the controller's normal requeue retries once
   `CertManagerInstallation` has caught up — no new retry mechanism, reusing
   what's already there.
3. Build (not yet apply) two hand-built objects: a `snapshot-controller`
   namespace (the chart renders none, same as
   `tigera-operator`/`spegel`/`cert-manager`) and a self-signed `Issuer`
   named `snapshot-controller-selfsigned` in that namespace — the exact
   recipe from the chart's own README (`spec: { selfSigned: {} }`), not an
   invented shape. The namespace gets no `pod-security.kubernetes.io/*`
   labels: the controller's own `securityContext` drops all capabilities and
   runs non-root, but (unlike cert-manager's chart) doesn't set
   `seccompProfile` explicitly, so this design assumes Talos's default
   `baseline` policy admits it unmodified — a stated assumption, confirmed or
   corrected live in the runbook, the same pattern `CertManagerInstallation`'s
   own spec already used for its namespace-admission assumption.
4. Render the chart (`helm template`, classic repo
   `https://piraeus.io/helm-charts/`, chart `snapshot-controller`) with the
   forced values from the API section above layered over `helmValues`,
   `webhook.tls.certManagerIssuerRef` pointing at the `Issuer` name from
   step 3.
5. Parse the rendered objects, prepend the two hand-built objects from step
   3, sort the combined set by rank. Checkpoint the ledger against this full
   desired set *before* applying anything — the same "everything about to be
   applied is known and persisted first" invariant every other reconciler
   already follows — then apply in rank order and prune anything no longer
   in the desired set.
6. Write `Ready` status; requeue at 300s.

### Cleanup

Standard finalizer, reverse ledger order. Since `installCRDs: true` is
forced, deleting this CR **cascades to delete every `VolumeSnapshot`,
`VolumeSnapshotContent`, `VolumeSnapshotClass` and their group-snapshot
counterparts cluster-wide**, the same class of hazard
`CertManagerInstallation`'s own cleanup already accepts and documents for its
six CRDs, not coded around. `deploy/README.md` and the runbook state this
plainly.

### Wiring

- `main.rs` builds a sixth watcher and `Controller<SnapshotController>`, same
  generation/deletion-requested/finalizer filter as every other loop, sharing
  the one `Context` and leader lease.
- The finalizer name (`platform.rye.ninja/cleanup`) is reused.
- `leader_gate` is reused from `reconciler.rs`.

### Code layout

- `src/snapshot_controller.rs`: CRD types (`SnapshotController`,
  `SnapshotControllerSpec`, `SnapshotControllerStatus`), validation, the
  values builder, and the `Issuer` object builder.
- `src/snapshot_controller_reconciler.rs`: reconcile, cleanup, finalizer
  wiring, status — same shape as `cert_manager_reconciler.rs`.
- `src/helm.rs`: new `SNAPSHOT_CONTROLLER_NAMESPACE` const, `PIRAEUS_CHART_REPO`
  const, and `SNAPSHOT_CONTROLLER_CHART: ChartRef` (classic repo form).
- `src/crds.rs`: `generated_yaml` gains the sixth CRD.
- `src/bin/crdgen.rs`: unchanged (delegates to `crds::generated_yaml`).

### Deployment and docs

- `deploy/crd.yaml` regenerated with all six CRDs; `deploy/README.md`'s
  `kubectl wait --for=condition=established` list gains
  `snapshotcontrollers.platform.rye.ninja`.
- No RBAC change (cluster-admin already covers it); the RBAC ledger memory
  gains a row.
- `examples/snapshot-controller.yaml`: a Talos starting point, `name:
  default`, a pinned `chartVersion`.
- New `deploy/README.md` section: apply **after** `CertManagerInstallation`
  is `Ready` (hard dependency, step 2 above) and after `CniInstallation`
  (pods run on the pod network and need cluster DNS). No dependency on
  `CloudControllerManager` or `CsiDriver` — but installing it makes
  `CsiDriver`'s `csi-snapshotter` sidecar's log noise stop, so listing it near
  `CsiDriver` in the apply-order doc is still useful context.
- `docs/runbooks/snapshot-controller-verification.md`: namespace admission
  under `baseline` PSS; the self-signed `Issuer`/`Certificate` issuing
  correctly (confirms the webhook's own TLS, not yet the conversion path);
  a real `VolumeSnapshot` created against a Cinder PVC and bound
  (`readyToUse: true`), proving the `CsiDriver` sidecar integration this
  whole slice exists for; a `VolumeGroupSnapshot` round-trip through the
  conversion webhook; delete/cleanup, including the CRD cascade-delete
  caveat.
- A `docs/memory/` entry and index line, per `CLAUDE.md`; the queued
  `csi-snapshot-support-2026-09` memory gets superseded/updated to point here
  rather than left dangling as "queued".

## Testing

- **Unit:** spec deserialization and defaults; each validation rejection
  (including the three new `InvalidHelmValues` cases for `installCRDs`,
  `webhook.enabled`, `webhook.tls.*`); values builder (forced fields always
  win over `helmValues`); the `Issuer` object builder.
- **Real-chart (ignored, needs network), in the style of the CCM/Cinder
  real-chart tests:** render chart `5.3.0` with the controller's values and
  assert all six CRDs, the webhook Deployment, the controller Deployment, and
  that `certManagerIssuerRef` reaches the rendered `Certificate` object
  correctly. Re-run on every chart bump.
- **Example:** the example manifest parses and validates, in the style of
  `tests/cloud_controller_manager_example.rs`.
- **Integration (ignored, Talos-in-Docker):** apply
  `CertManagerInstallation` then this CR, assert the controller/webhook
  Deployments and the `Issuer`/`Certificate` appear and the `Certificate`
  reaches `Ready`, delete the CR, assert the applied resources (including the
  `Issuer`) are gone. Also assert that applying this CR **before**
  `CertManagerInstallation` surfaces `Failed`/`ApplyFailed` rather than
  hanging or erroring some other way.
- **Live acceptance (manual, real cluster, runbook):** namespace admission;
  the self-signed `Issuer`/`Certificate` smoke test; a real `VolumeSnapshot`
  against a live Cinder PVC, confirmed `readyToUse: true` and a real Cinder
  snapshot exists via the Cinder API directly (not just Kubernetes status,
  matching how the Cinder PVC provisioning claim was verified); a
  `VolumeGroupSnapshot` conversion-webhook round-trip; delete behavior and
  the CRD cascade-delete confirmed for real.

## Verification status

Implemented. All six tasks are merged and the full Rust test suite passes
(283 tests, 0 failed, 9 ignored), including the ignored real-chart test in
`src/helm.rs` run with `--ignored` against the live `piraeus.io` chart --
the CRD list, Deployment/Service/Certificate shape and feature-gate default
described above are live-verified facts, not assumptions. Cluster
verification (the runbook, `docs/runbooks/snapshot-controller-verification.md`)
has not yet been run.
