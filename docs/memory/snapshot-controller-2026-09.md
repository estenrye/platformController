---
name: snapshot-controller-2026-09
description: SnapshotController slice, 2026-09-29 - sixth CRD, resumes the queued csi-snapshot-support sub-project's CRD/controller/webhook half; live-verified on a real Talos cluster; group snapshots made an opt-in, off-by-default toggle after live testing found them non-functional on OpenStack (no driver support, and the conversion webhook itself errors)
metadata:
  type: project
---

`SnapshotController` (cluster-scoped singleton `default`, shortname
`snapctl`) installs the cluster-wide CSI VolumeSnapshot support -- the
`snapshot.storage.k8s.io` CRDs and the `snapshot-controller` itself -- that
[[csi-driver-openstack-cinder-2026-09]]'s own `csi-snapshotter` sidecar needs
but does not install. Spec:
`docs/superpowers/specs/2026-09-29-snapshot-controller-design.md`; plan:
`docs/superpowers/plans/2026-09-29-snapshot-controller.md`; live acceptance:
`docs/runbooks/snapshot-controller-verification.md`. Resumes
[[csi-snapshot-support-2026-09]]'s CRD/controller/webhook half -- see that
memory for what changed from its research (a real Helm chart exists after
all).

**Correction, same day, found by live-deploying to a real cluster:** the
initial design forced group-snapshot support (the conversion webhook, a
self-signed cert-manager `Issuer`, `CertManagerInstallation` as a hard
dependency) on unconditionally, reasoning that the chart's own default
already enabled it. Deploying it for real found two independent problems:
(1) `kubernetes/cloud-provider-openstack`'s `cinder-csi-plugin` has never
implemented the CSI group-snapshot RPCs at all (confirmed by searching that
project's entire source for `GroupControllerServer`/
`CreateVolumeGroupSnapshot` -- zero matches, any version), and (2) the
deployed conversion webhook itself errors on every conversion attempt
(`unexpected conversion version from "groupsnapshot.storage.k8s.io/v1" to
"...v1beta2"`), worse than a no-op. `spec.groupSnapshotsEnabled` (typed
bool, default `false`) was added so this is a per-deployment choice instead
of a hardcoded one -- a future platform/driver that does implement group
snapshots can opt in without a controller code change. See "Verification
status" below for the live evidence.

**Non-obvious facts, from rendering the real chart (`5.3.0`), not assumed:**
- `kubernetes-csi/external-snapshotter` genuinely has no official Helm
  chart, but `piraeusdatastore/helm-charts`' `snapshot-controller` chart
  (classic repo `https://piraeus.io/helm-charts/`) is a real, actively
  maintained third-party one sourced directly from that upstream project --
  the existing `helm.rs` render pipeline (`ChartSource::Repo`) needed zero
  new code to support it, contradicting the queued sub-project's
  "needs a new raw-YAML render path" assumption.
- `installCRDs: true` renders exactly 6 CRDs regardless of
  `groupSnapshotsEnabled`: the 3 core
  (`volumesnapshotclasses`/`volumesnapshots`/`volumesnapshotcontents`.
  `snapshot.storage.k8s.io`) plus 3 group-snapshot ones
  (`volumegroupsnapshotclasses`/`volumegroupsnapshotcontents`/
  `volumegroupsnapshots`.`groupsnapshot.storage.k8s.io`) -- `installCRDs` is
  a single all-or-nothing toggle, so the group-snapshot CRDs just sit unused
  when the flag is off. The queued research had recommended skipping them
  as YAGNI; turned out right in practice (see the Correction above), just
  not achievable by omitting them from `installCRDs` specifically.
- The chart renders no `Namespace` and no `Secret` (the webhook's TLS Secret
  is produced later, at runtime, by cert-manager's own `Certificate`
  controller, not by `helm template`).
- With `groupSnapshotsEnabled: false` (the default): 1 Deployment
  (`snapshot-controller`), no Service, no Certificate, no CRD conversion
  block at all (defaults to `strategy: None`, matching upstream). With
  `groupSnapshotsEnabled: true`: 2 Deployments (adds
  `snapshot-controller-conversion-webhook`), 1 Service (webhook only -- the
  controller serves no traffic). RBAC (1 ClusterRole/ClusterRoleBinding
  pair, 1 Role/RoleBinding pair for leader-election `Lease` access) and 2
  ServiceAccounts render either way.
- The chart's "webhook" is a CRD **conversion** webhook (wired via each
  CRD's own `spec.conversion.webhook`), not a separate
  `Validating`/`MutatingWebhookConfiguration` object -- it only matters for
  clusters holding `VolumeGroupSnapshot` objects in old v1beta1/v1beta2 API
  versions, which a fresh install never has, and (see the Correction above)
  the deployed version of it doesn't even work for that. When
  `groupSnapshotsEnabled: true`, it's wired up following the chart's own
  README recipe for cert-manager-backed TLS: a namespaced self-signed
  `Issuer`, not a `ClusterIssuer` -- this component applies that `Issuer`
  itself, tracked in its own ledger, not shared with [[cert-manager-2026-09]]
  (which deliberately configures none). This carries an implicit dependency
  on cert-manager's `cainjector` being enabled and running to populate the
  conversion webhook's CA bundle -- `CertManagerInstallation`'s `helmValues`
  passthrough does not force this on. Live-verified: the CA bundle populates
  correctly and the `Issuer`/`Certificate` both reach `Ready=True` -- the
  webhook's *own* TLS plumbing genuinely works, it's the conversion logic
  itself that's broken.
- **This is the first component with a hard, same-reconcile dependency on
  another component's CRDs actually being registered** -- but only when
  `groupSnapshotsEnabled: true`; with the default `false` there's no
  `CertManagerInstallation` dependency at all. When it applies, it's
  handled with zero new code: the `Issuer` (hand-built) and the chart's own
  `Certificate` are both `cert-manager.io` custom resources, so the existing
  `is_custom_resource`/`wait_for_object_kind` mechanism already used for
  `CsiDriver`'s `StorageClass`/`CSIDriver` objects and `CniInstallation`'s
  wait on the tigera operator's own CRDs covers this for free. Applying
  `SnapshotController` (with the flag on) before [[cert-manager-2026-09]]
  surfaces `Failed`/`ApplyFailed` (not a bespoke reason) and retries
  automatically.
- **Group snapshots don't actually work, live-confirmed two independent
  ways.** No version of `cinder-csi-plugin` implements the CSI
  group-snapshot RPCs (`GroupControllerServer`/`CreateVolumeGroupSnapshot`
  absent from `kubernetes/cloud-provider-openstack`'s entire source, not
  just the pinned version -- this needs an upstream PR, not a change here).
  Separately and even if that weren't true, the deployed conversion webhook
  itself rejects every conversion it's asked to perform
  (`unexpected conversion version from "groupsnapshot.storage.k8s.io/v1" to
  "...v1beta2"`), so a `VolumeGroupSnapshot` retries forever rather than
  ever reaching `readyToUse: true`. This is why `groupSnapshotsEnabled`
  defaults `false`.
- Controller and webhook containers drop every capability and run
  non-root, but -- unlike cert-manager's chart -- set no explicit
  `seccompProfile`. The synthesized namespace carries no
  `pod-security.kubernetes.io/*` labels (matching cert-manager's
  namespace) -- live-verified 2026-09-29 against a real Talos cluster
  (runbook step 2): no admission rejection, both pods Running under the
  default `baseline` policy despite the weaker (no-`seccompProfile`)
  assumption.
- Chart and app versions do **not** track together (`5.3.0` ships app
  `v8.6.0`), like the OpenStack charts and unlike cert-manager.
- This is a classic-repo chart (`ChartSource::Repo`), not OCI, so
  `strip_oci_pull_preamble` never applies to it.
- A `helmValues` attempt to set `installCRDs`, `webhook.enabled`, anything
  under `webhook.tls`, or `controller.args.featureGates` is **rejected** at
  validation (`InvalidHelmValues`), unlike `CertManagerInstallation`'s
  `crds.enabled` (silently overridden) -- the only way to change any of
  these is the typed `groupSnapshotsEnabled` toggle, which also gates the
  `Issuer`/dependency logic, so a passthrough can't get them out of sync.
- Deleting a `SnapshotController` cascades to delete all six CRDs and
  therefore every `VolumeSnapshot`/`VolumeGroupSnapshot`-family object
  cluster-wide, the same class of hazard [[cert-manager-2026-09]] already
  documented for its own six CRDs.
- No typed `VolumeSnapshotClass` support here (or on `CsiDriver`) --
  deliberately descoped during brainstorming; see the design spec's
  Non-goals. The runbook's live `VolumeSnapshot` smoke test has to create
  one by hand for that reason.

**Verification status:** fully live-verified, including the correction. All
Rust code built and the full test suite passed (290 lib tests, 0 failed, 10
ignored -- up from a 256-test baseline before this slice), including both
ignored real-chart tests in `src/helm.rs`
(`snapshot_controller_chart_renders_the_shape_the_spec_relies_on` for the
`groupSnapshotsEnabled: false` default,
`snapshot_controller_chart_renders_group_snapshot_support_when_enabled` for
`true`), both run with `--ignored` against the live `piraeus.io` chart.
Merged via #27 (component), #28 (the `0.1.9` image pin bump needed to
actually run it), and #29 (recording the findings below).

Live-verified 2026-09-29 on a real 6-node Talos cluster (controller `0.1.9`,
chart `5.3.0`), alongside `CniInstallation`/`CertManagerInstallation`/
`CsiDriver` which stayed `Ready` throughout the controller rollout from
`0.1.8` -- this live run predates the `groupSnapshotsEnabled` toggle, i.e.
ran with the webhook forced on (what prompted adding the toggle):

- Apply reached `Ready` in seconds; namespace admission held with no
  `pod-security.kubernetes.io/*` labels -- the weaker, no-`seccompProfile`
  assumption turned out fine in practice. No disruption to the other four
  components during the rollout.
- A real `VolumeSnapshot` against a throwaway 1Gi Cinder PVC
  (`csi-cinder-sc-delete`) reached `readyToUse: true` in ~10 seconds. The
  `VolumeSnapshotContent` carries a real Cinder-assigned `snapshotHandle`
  UUID, populated by the CSI driver's own provisioning call -- confirms
  the end-to-end path this whole component exists for actually works, not
  just "manifests applied". Direct `openstack volume snapshot show`
  confirmation wasn't possible (no `openstack` CLI/credentials on the
  verifying machine); the `snapshotHandle` is the corroborating evidence
  in its place. All test resources were cleaned up and confirmed gone.
- The self-signed `Issuer`/`Certificate` reached `Ready=True` with a
  populated `caBundle` (1456 bytes) -- the webhook's own TLS plumbing
  genuinely works. But every `VolumeGroupSnapshot` attempt failed with
  `unexpected conversion version from "groupsnapshot.storage.k8s.io/v1" to
  "groupsnapshot.storage.k8s.io/v1beta2"`, retried forever -- this and the
  missing `CREATE_DELETE_GROUP_SNAPSHOT` driver capability (confirmed via
  both the live pod's own startup log and an upstream source search) are
  what prompted the `groupSnapshotsEnabled` toggle and its `false` default.
- **Not yet run against the corrected code:** the one-Deployment shape
  `groupSnapshotsEnabled: false` now produces (only verified locally via
  the ignored real-chart test, not yet against this live cluster), and step
  5 (delete `SnapshotController`, confirm the six-CRD cascade). The
  component was left installed and `Ready` rather than torn down.

Full steps and exact commands: `docs/runbooks/snapshot-controller-verification.md`.

**How to apply:** when bumping the chart version, re-render it
(`helm template snapshot-controller --repo https://piraeus.io/helm-charts/
snapshot-controller --version <new> --include-crds --no-hooks --namespace
snapshot-controller --values <the same typed values build_values sets>`)
and re-check the CRD list, the Deployment/Service/Certificate shape, and the
feature-gate default; re-run the ignored real-chart test in `helm.rs`.
