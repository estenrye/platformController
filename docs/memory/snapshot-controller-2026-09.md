---
name: snapshot-controller-2026-09
description: SnapshotController slice, 2026-09-29 - sixth CRD, resumes the queued csi-snapshot-support sub-project's CRD/controller/webhook half; chart facts confirmed by live rendering (not yet run against a cluster)
metadata:
  type: project
---

`SnapshotController` (cluster-scoped singleton `default`, shortname
`snapctl`) installs the cluster-wide CSI VolumeSnapshot support -- the
`snapshot.storage.k8s.io`/`groupsnapshot.storage.k8s.io` CRDs and the
`snapshot-controller` itself -- that [[csi-driver-openstack-cinder-2026-09]]'s
own `csi-snapshotter` sidecar needs but does not install. Spec:
`docs/superpowers/specs/2026-09-29-snapshot-controller-design.md`; plan:
`docs/superpowers/plans/2026-09-29-snapshot-controller.md`; live acceptance:
`docs/runbooks/snapshot-controller-verification.md`. Resumes
[[csi-snapshot-support-2026-09]]'s CRD/controller/webhook half -- see that
memory for what changed from its research (a real Helm chart exists after
all; group-snapshot support and the conversion webhook are in scope here,
which that research had leaned toward skipping).

**Non-obvious facts, from rendering the real chart (`5.3.0`), not assumed:**
- `kubernetes-csi/external-snapshotter` genuinely has no official Helm
  chart, but `piraeusdatastore/helm-charts`' `snapshot-controller` chart
  (classic repo `https://piraeus.io/helm-charts/`) is a real, actively
  maintained third-party one sourced directly from that upstream project --
  the existing `helm.rs` render pipeline (`ChartSource::Repo`) needed zero
  new code to support it, contradicting the queued sub-project's
  "needs a new raw-YAML render path" assumption.
- `installCRDs: true` renders exactly 6 CRDs: the 3 core
  (`volumesnapshotclasses`/`volumesnapshots`/`volumesnapshotcontents`.
  `snapshot.storage.k8s.io`) plus 3 group-snapshot ones
  (`volumegroupsnapshotclasses`/`volumegroupsnapshotcontents`/
  `volumegroupsnapshots`.`groupsnapshot.storage.k8s.io`) -- the queued
  research had recommended skipping the latter three as YAGNI; the approved
  design chose to include them instead, since the chart's own default
  (`--feature-gates=CSIVolumeGroupSnapshot=true`) already turns them on and
  excluding them would mean actively fighting the chart.
- The chart renders no `Namespace` and no `Secret` (the webhook's TLS Secret
  is produced later, at runtime, by cert-manager's own `Certificate`
  controller, not by `helm template`).
- Exactly 2 Deployments (`snapshot-controller`,
  `snapshot-controller-conversion-webhook`), 1 Service (webhook only -- the
  controller serves no traffic), 1 ClusterRole/ClusterRoleBinding pair, 1
  Role/RoleBinding pair (leader-election `Lease` access), 2 ServiceAccounts.
- The chart's "webhook" is a CRD **conversion** webhook (wired via each
  CRD's own `spec.conversion.webhook`), not a separate
  `Validating`/`MutatingWebhookConfiguration` object -- it only matters for
  clusters holding `VolumeGroupSnapshot` objects in old v1beta1/v1beta2 API
  versions, which a fresh install never has. Enabled anyway here (group
  snapshots in scope), following the chart's own README recipe for
  cert-manager-backed TLS: a namespaced self-signed `Issuer`, not a
  `ClusterIssuer` -- this component applies that `Issuer` itself, tracked in
  its own ledger, not shared with [[cert-manager-2026-09]] (which
  deliberately configures none). This carries an implicit dependency on
  cert-manager's `cainjector` being enabled and running to populate the
  conversion webhook's CA bundle -- `CertManagerInstallation`'s `helmValues`
  passthrough does not force this on.
- **This is the first component with a hard, same-reconcile dependency on
  another component's CRDs actually being registered**, unlike every prior
  ordering note in this codebase (all of which were "eventually consistent"
  Pod-scheduling delays, not apply-time failures). Handled with zero new
  code: the `Issuer` (hand-built) and the chart's own `Certificate` are both
  `cert-manager.io` custom resources, so the existing
  `is_custom_resource`/`wait_for_object_kind` mechanism already used for
  `CsiDriver`'s `StorageClass`/`CSIDriver` objects and `CniInstallation`'s
  wait on the tigera operator's own CRDs covers this for free. Applying
  `SnapshotController` before [[cert-manager-2026-09]] surfaces
  `Failed`/`ApplyFailed` (not a bespoke reason) and retries automatically.
- Controller and webhook containers drop every capability and run
  non-root, but -- unlike cert-manager's chart -- set no explicit
  `seccompProfile`. The synthesized namespace is designed to carry no
  `pod-security.kubernetes.io/*` labels anyway (matching cert-manager's
  namespace), but this is a **stated assumption pending the runbook's step
  2**, weaker than cert-manager's live-confirmed one.
- Chart and app versions do **not** track together (`5.3.0` ships app
  `v8.6.0`), like the OpenStack charts and unlike cert-manager.
- This is a classic-repo chart (`ChartSource::Repo`), not OCI, so
  `strip_oci_pull_preamble` never applies to it.
- A `helmValues` attempt to set `installCRDs`, `webhook.enabled` or anything
  under `webhook.tls` is **rejected** at validation
  (`InvalidHelmValues`), unlike `CertManagerInstallation`'s `crds.enabled`
  (silently overridden) -- a webhook TLS misconfiguration fails loudly here
  instead of quietly.
- Deleting a `SnapshotController` cascades to delete all six CRDs and
  therefore every `VolumeSnapshot`/`VolumeGroupSnapshot`-family object
  cluster-wide, the same class of hazard [[cert-manager-2026-09]] already
  documented for its own six CRDs.
- No typed `VolumeSnapshotClass` support here (or on `CsiDriver`) --
  deliberately descoped during brainstorming; see the design spec's
  Non-goals. The runbook's live `VolumeSnapshot` smoke test (step 4) has to
  create one by hand for that reason.

**Verification status:** implemented and code-verified, not yet
cluster-verified. All six tasks are merged. The full test suite passes (283
tests, 0 failed, 9 ignored -- up from a 256-test baseline before this slice),
including the ignored real-chart test in `src/helm.rs`
(`snapshot_controller_chart_renders_the_shape_the_spec_relies_on`), run with
`--ignored` against the live `piraeus.io` chart and passing. **Not yet
verified against a real cluster** -- the runbook
(`docs/runbooks/snapshot-controller-verification.md`) is written but has not
been run.

**How to apply:** when bumping the chart version, re-render it
(`helm template snapshot-controller --repo https://piraeus.io/helm-charts/
snapshot-controller --version <new> --include-crds --no-hooks --namespace
snapshot-controller --values <the same typed values build_values sets>`)
and re-check the CRD list, the Deployment/Service/Certificate shape, and the
feature-gate default; re-run the ignored real-chart test in `helm.rs`.
