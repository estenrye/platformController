# SnapshotController (CSI Volume Snapshot Support)

Status: Implemented, live-verified
Date: 2026-09-29

**Update 2026-09-29 (post-live-verification):** the design below as
originally written forced group-snapshot support (the conversion webhook,
the self-signed `Issuer`, `CertManagerInstallation` as a hard dependency) on
unconditionally. Deploying it to a real cluster found two independent,
live-confirmed problems: (1) `kubernetes/cloud-provider-openstack`'s
`cinder-csi-plugin` has never implemented the CSI group-snapshot RPCs
(`CreateVolumeGroupSnapshot` etc. — confirmed absent anywhere in that
project's source), so no CSI driver this controller supports today can act
on a `VolumeGroupSnapshot` at all; and (2) the deployed conversion webhook
itself actively errors on every conversion attempt
(`unexpected conversion version from "groupsnapshot.storage.k8s.io/v1" to
"...v1beta2"`), which is strictly worse than a no-op — a `VolumeGroupSnapshot`
retries forever rather than failing cleanly. Rather than removing
group-snapshot support outright (hard-coding "off" in the binary would mean
a future platform/driver that does implement it needs a code change and
redeploy to turn it back on), `spec.groupSnapshotsEnabled` was added: a
typed, per-deployment toggle, defaulting to `false`. The body below is
updated in place to describe the toggle; passages describing the
now-superseded "always on" behavior have been corrected, not left as
historical record — this is a behavior spec, and stale behavior claims here
would mislead a future reader. The full live-testing narrative lives in
`docs/memory/snapshot-controller-2026-09.md`.

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
that missing cluster-level piece: the CRDs and the controller, always. Group
snapshot support — a conversion webhook plus a self-signed `cert-manager`
`Issuer`/`Certificate` for its TLS — is optional, off by default
(`spec.groupSnapshotsEnabled: false`), since no CSI driver this controller
supports today implements it (see the Update note above). It resumes the
queued sub-project B recorded in [[csi-snapshot-support-2026-09]]
(sub-project A, typed `StorageClass` customization on `CsiDriver`, already
shipped), but departs from that project's research in one material way: that
research assumed `kubernetes-csi/external-snapshotter` had no Helm chart and
would need a new raw-YAML render path. It does have one —
`piraeusdatastore/helm-charts`' `snapshot-controller` chart, sourced directly
from the same upstream project — so this design reuses the existing `helm.rs`
render pipeline unchanged, the same way every other component does.

When `groupSnapshotsEnabled: true`, this component depends on
`CertManagerInstallation` (already shipped, [[cert-manager-2026-09]]) being
applied first, for the `Issuer`/`Certificate` CRDs the webhook's TLS setup
needs. `CertManagerInstallation` itself deliberately installs no
`Issuer`/`ClusterIssuer` — this is the "future, separate component" its own
Non-goals pointed at. With the default `false`, this component has no
dependency on `CertManagerInstallation` at all.

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
  When `groupSnapshotsEnabled: true`, this component's reconcile waits for
  the `Issuer` kind to become available (see Reconcile) and surfaces that as
  a retried `Failed` status if `CertManagerInstallation` isn't there yet; it
  does not check for `CertManagerInstallation`'s existence as a CR, the same
  "no cross-CR ownership checks" stance `CsiDriver` already takes toward
  `CloudControllerManager`'s taint-clearing.
- **Making group snapshots work on OpenStack.** Not fixable here: no version
  of `cinder-csi-plugin` implements the CSI group-snapshot RPCs (confirmed
  by searching `kubernetes/cloud-provider-openstack`'s own source,
  2026-09-29). This would need an upstream PR merged into that project, not
  a change on this controller's side.
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
  platformKind: talos-linux         # only value accepted today
  chartVersion: "5.3.0"             # required, no default, same as every other chart
  groupSnapshotsEnabled: false      # optional, default false
  helmValues: {}                    # optional free-form passthrough
```

- `chartVersion` is the `snapshot-controller` chart's own version (chart and
  app versions do not track together on this chart — `5.3.0` renders app
  `v8.6.0` — so this needs live confirmation and a note in the example, the
  same caution the Cinder spec already applies to chart-vs-app version
  drift).
- `groupSnapshotsEnabled` is a typed, per-deployment toggle for the CRD
  conversion webhook and its self-signed `Issuer`, defaulting to `false`.
  Live-tested with it forced on: two independent problems, both confirmed on
  a real cluster (see the Update note above) — no supported driver
  implements the group-snapshot RPCs, and the deployed conversion webhook
  itself errors on every attempt rather than sitting idle. Defaulting off
  avoids both by default while leaving a real path to opt in once a
  driver/platform actually supports it, without a controller code change.
- `helmValues` is merged first; the controller's own typed values are
  overlaid afterwards, so a passthrough can never disable or contradict them.
  Always forced: `installCRDs: true`. Driven by `groupSnapshotsEnabled`:
  `webhook.enabled` (`false` by default, `true` when enabled, along with
  `webhook.tls.autogenerate: false` and `webhook.tls.certManagerIssuerRef:
  {name: snapshot-controller-selfsigned, kind: Issuer}`) and
  `controller.args.featureGates` (`""` by default, `"CSIVolumeGroupSnapshot=true"`
  when enabled). Rejected as `InvalidHelmValues` if `helmValues` sets any of
  `installCRDs`, `webhook.enabled`, `webhook.tls.*`, or
  `controller.args.featureGates` — the same "don't silently let a passthrough
  reintroduce a broken state" rule `CsiDriver` applies to `secret.data`, made
  an explicit rejection here rather than a silent overlay-and-ignore, since
  the only way to change these is the typed toggle that also gates the
  `Issuer`/dependency logic.
- No `cleanupTimeoutSeconds`: no DaemonSet-shaped, node-resident resources
  need a bounded wait on removal, same as `CertManagerInstallation`.

Validation, rejected with `phase: Failed` and a `reason`, same pattern as
every other component:

- the name is not `default` — `Unsupported`
- `platformKind` is unsupported — `Unsupported`
- `chartVersion` is empty, or has leading/trailing whitespace —
  `InvalidChartVersion`
- `helmValues` is not a JSON object, or sets `installCRDs` /
  `webhook.enabled` / any `webhook.tls.*` key / `controller.args.featureGates`
  — `InvalidHelmValues`

Status mirrors every other kind and reuses `Phase`, `Condition` and
`AppliedResourceRef`: `phase`, `observedGeneration`, `chartVersion`,
`appliedResources`, `conditions`.

### Reconcile

1. Leader gate, then validate; write `Failed` status on rejection.
2. Build (not yet apply) the `snapshot-controller` namespace object (the
   chart renders none, same as `tigera-operator`/`spegel`/`cert-manager`).
   The namespace gets no `pod-security.kubernetes.io/*` labels: the
   controller's own `securityContext` drops all capabilities and runs
   non-root, but (unlike cert-manager's chart) doesn't set `seccompProfile`
   explicitly — live-verified 2026-09-29 against a real Talos cluster: no
   admission rejection either way.
3. **Only when `groupSnapshotsEnabled: true`:** build a self-signed `Issuer`
   named `snapshot-controller-selfsigned` in that namespace — the exact
   recipe from the chart's own README (`spec: { selfSigned: {} }`), not an
   invented shape — and wait for the `Issuer` kind (`cert-manager.io/v1`) to
   be available via the existing `apply::wait_for_kind_available`, the same
   utility `CniInstallation` already uses to wait for the tigera operator's
   own CRDs. This is a hard dependency when the flag is set: applying an
   `Issuer` object before its CRD exists is a discovery failure, not an
   eventually-consistent Pending pod like `CsiDriver`'s ordering against
   `CloudControllerManager`. Timing out here writes `Failed`/`ApplyFailed`
   (the same reason `ApplyError::KindNotAvailable` already maps to, no new
   variant or reason string) and the controller's normal requeue retries
   once `CertManagerInstallation` has caught up. When the flag is `false`
   (the default), none of this runs and there is no `CertManagerInstallation`
   dependency at all.
4. Render the chart (`helm template`, classic repo
   `https://piraeus.io/helm-charts/`, chart `snapshot-controller`) with the
   forced values from the API section above layered over `helmValues` —
   `webhook.tls.certManagerIssuerRef` pointing at the `Issuer` name from
   step 3 only when `groupSnapshotsEnabled: true`.
5. Parse the rendered objects, prepend the namespace (and, when enabled, the
   `Issuer`) from steps 2-3, sort the combined set by rank. Checkpoint the
   ledger against this full desired set *before* applying anything — the
   same "everything about to be applied is known and persisted first"
   invariant every other reconciler already follows — then apply in rank
   order and prune anything no longer in the desired set.
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
  default`, a pinned `chartVersion`, `groupSnapshotsEnabled: false`.
- New `deploy/README.md` section: apply after `CniInstallation` (pods run on
  the pod network and need cluster DNS). No dependency on
  `CertManagerInstallation`, `CloudControllerManager` or `CsiDriver` with the
  default `groupSnapshotsEnabled: false` — but installing it makes
  `CsiDriver`'s `csi-snapshotter` sidecar's log noise stop, so listing it near
  `CsiDriver` in the apply-order doc is still useful context. Enabling group
  snapshots adds a hard `CertManagerInstallation` dependency, documented
  where the flag itself is documented.
- `docs/runbooks/snapshot-controller-verification.md`: namespace admission
  under `baseline` PSS; a real `VolumeSnapshot` created against a Cinder PVC
  and bound (`readyToUse: true`), proving the `CsiDriver` sidecar integration
  this whole slice exists for; delete/cleanup, including the CRD
  cascade-delete caveat. Group-snapshot-specific steps (the self-signed
  `Issuer`/`Certificate`, a `VolumeGroupSnapshot` attempt) are documented as
  a known-broken path, not a passing verification step.
- A `docs/memory/` entry and index line, per `CLAUDE.md`; the queued
  `csi-snapshot-support-2026-09` memory gets superseded/updated to point here
  rather than left dangling as "queued".

## Testing

- **Unit:** spec deserialization and defaults (`groupSnapshotsEnabled`
  defaults `false`); each validation rejection (including the
  `InvalidHelmValues` cases for `installCRDs`, `webhook.enabled`,
  `webhook.tls.*`, `controller.args.featureGates`); values builder for both
  branches of `groupSnapshotsEnabled` (forced fields always win over
  `helmValues`); the `Issuer` object builder.
- **Real-chart (ignored, needs network), in the style of the CCM/Cinder
  real-chart tests:** two tests — `groupSnapshotsEnabled: false` (the
  default) asserts all six CRDs, exactly one Deployment, no Service, no
  `Certificate`, no CRD conversion block, and an empty feature-gates flag;
  `groupSnapshotsEnabled: true` asserts the webhook Deployment/Service
  appear and `certManagerIssuerRef` reaches the rendered `Certificate`
  object correctly, noting the known-broken conversion path in a code
  comment rather than asserting it works. Re-run both on every chart bump.
- **Example:** the example manifest parses and validates, in the style of
  `tests/cloud_controller_manager_example.rs`.
- **Integration (ignored, Talos-in-Docker):** apply this CR with the default
  `groupSnapshotsEnabled: false`, assert the controller Deployment appears,
  delete the CR, assert the applied resources are gone. No
  `CertManagerInstallation` prerequisite needed for this path.
- **Live acceptance (manual, real cluster, runbook):** namespace admission; a
  real `VolumeSnapshot` against a live Cinder PVC, confirmed `readyToUse:
  true` via a real Cinder-assigned `snapshotHandle`; delete behavior and the
  CRD cascade-delete. Group snapshots are documented as a known-broken,
  opt-in path, not a passing verification step.

## Verification status

Implemented and live-verified, including the `groupSnapshotsEnabled` toggle.
All six original tasks plus this correction are merged; the full Rust test
suite passes, including both ignored real-chart tests in `src/helm.rs` run
with `--ignored` against the live `piraeus.io` chart. Live-verified against a
real 6-node Talos cluster 2026-09-29: with the default `groupSnapshotsEnabled:
false`, `Ready` in seconds, namespace admission held, and a real
`VolumeSnapshot` against a Cinder PVC reached `readyToUse: true` with a
genuine Cinder-assigned `snapshotHandle`. With `groupSnapshotsEnabled: true`
(tested before the toggle was added, prompting it): the CRD/controller/
webhook infrastructure came up correctly (`Issuer`/`Certificate` both
`Ready=True`, `cainjector` populated the CA bundle), but every
`VolumeGroupSnapshot` attempt failed with a conversion-webhook error, and
separately no CSI driver this controller supports implements the
group-snapshot RPCs at all — see the Update note above. Full write-up:
`docs/memory/snapshot-controller-2026-09.md`.
