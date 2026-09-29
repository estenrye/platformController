---
name: cert-manager-2026-09
description: CertManagerInstallation slice, 2026-09-28/29 - fifth CRD, no provider enum, live-verified on a real Talos cluster (apply/Ready, namespace admission, self-signed smoke test, delete/cleanup all pass)
metadata:
  type: project
---

`CertManagerInstallation` (cluster-scoped singleton `default`, shortname
`certmgr`) installs cert-manager on Talos. Spec:
`docs/superpowers/specs/2026-09-28-cert-manager-installation-design.md`;
plan: `docs/superpowers/plans/2026-09-28-cert-manager-installation.md`; live
acceptance: `docs/runbooks/cert-manager-verification.md`. Unlike every other
component here, its spec has **no `provider` field** -- there is no second
implementation of "the thing that installs cert-manager" to model, so the
spec is flat (`platformKind`, `chartVersion`, `helmValues`). It also
installs cert-manager only: no `ClusterIssuer`/`Issuer` configuration, which
is deliberately out of scope (see the spec's Non-goals) and left for a
future, separate component.

**Non-obvious facts, from rendering the real chart (`v1.16.2`), not
assumed:**
- `crds.enabled` **defaults to `false`** on this chart -- rendering without
  it produces zero CRDs. `build_values` forces it `true` unconditionally, a
  real requirement rather than defensive redundancy. With it true, 6 CRDs
  render: `certificaterequests`/`certificates`/`challenges.acme`/
  `clusterissuers`/`issuers`/`orders.acme`, all `.cert-manager.io`.
- The chart renders **no `Namespace`** and **no `Secret`** object, same as
  Calico's tigera-operator chart and Spegel.
- Exactly 3 Deployments (`cert-manager-cainjector`, `cert-manager`,
  `cert-manager-webhook`), 1 `ValidatingWebhookConfiguration`
  (`cert-manager-webhook`), 1 `MutatingWebhookConfiguration`.
- Every container sets `runAsNonRoot: true`, a `seccompProfile`,
  `allowPrivilegeEscalation: false` and drops every capability --
  **restricted**-PSS-safe, not merely baseline-safe. So
  `cert_manager_namespace_object` sets **no**
  `pod-security.kubernetes.io/*` labels, unlike `tigera-operator` (Calico)
  and `spegel` (both `privileged`, for hostPath/host-network reasons that
  don't apply here). Live-verified 2026-09-29 against a real Talos cluster
  (runbook step 2): no admission rejection, every pod Running under the
  default `baseline` policy.
- Like Spegel, the OCI chart prints `Pulled:`/`Digest:` preamble lines to
  stdout; `helm::strip_oci_pull_preamble` already handles this generically,
  no code change needed.
- Unlike the OpenStack charts (chart version has no `v` prefix, app version
  does), cert-manager's chart and app versions **track together and both
  carry `v`** (`v1.16.2`). Documented in the example and its test so this
  doesn't get "corrected" the wrong way by someone used to the CCM/CSI
  convention.
- No `cleanupTimeoutSeconds`, no `wait_for_object_kind` call needed: this
  component creates no `cert-manager.io` custom resource itself, so there's
  no same-reconcile ordering dependency the way CNI has on the tigera
  operator's own CRDs.
- **Deleting a `CertManagerInstallation` destroys every cert-manager custom
  resource in the cluster, not just ones this component manages.** Found in
  the final PR review (#24): the six `cert-manager.io` CRDs are in the
  applied-resources ledger (since `crds.enabled` is forced `true`), and
  Kubernetes deletes every instance of a kind when its CRD is deleted. Fixed
  by correcting `deploy/README.md` and this runbook to say so plainly
  (no code change -- deleting the CRDs along with everything else is
  accepted behavior, just previously mis-documented as safe). Live-verified
  2026-09-29 (runbook step 4): the cascade is real.
- **`crds.enabled` silently no-ops on cert-manager charts older than v1.15**
  (which used `installCRDs` instead) -- helm ignores the unknown value
  rather than erroring, so a `chartVersion` pinned to an old chart would
  render zero CRDs while still reporting `Ready`. Fixed in the same review
  pass: `reconcile_inner` now checks the rendered objects for at least one
  `CustomResourceDefinition` and fails `Failed`/`MissingCrds` otherwise
  (`renders_expected_crds` in `src/cert_manager_reconciler.rs`).

**Verification status:** fully live-verified. All Rust code built and the
full test suite passed (twice: once pre-review, once after the review fix
pass), including the two `#[ignore]`d real-chart tests in `helm.rs` against
the live `quay.io` chart. Merged via #24 (component) and #25 (the
`0.1.8` image pin bump needed to actually run it). Live-verified
2026-09-29 on a real 6-node Talos cluster (controller `0.1.8`, chart
`v1.16.2`), alongside `CniInstallation`/`CloudControllerManager`/
`CsiDriver` which stayed `Ready` throughout the controller rollout:

- Apply reached `Ready` in ~9s; all three Deployments Running.
- Namespace admission held with no `pod-security.kubernetes.io/*` labels
  (see the bullet above).
- The self-signed `ClusterIssuer`/`Certificate` smoke test issued a real
  `kubernetes.io/tls` Secret, `Ready=True` on the first poll -- the webhook
  and CA injection are genuinely functional, not just "manifests applied".
- Delete returned once the finalizer cleared; the namespace and all six
  CRDs were fully gone (confirming the cascade-delete finding above).
- Cert-manager was redeployed afterward and is running on the cluster as
  of this writing.

Full steps and exact commands: `docs/runbooks/cert-manager-verification.md`.

**How to apply:** when bumping the chart version, re-render it
(`helm template cert-manager oci://quay.io/jetstack/charts/cert-manager
--version <new> --include-crds --no-hooks --namespace cert-manager --set
crds.enabled=true`) and re-check the CRD list, the Deployment/webhook
counts, and whether a `Namespace`/`Secret`/hook object starts appearing;
re-run the two ignored real-chart tests in `helm.rs`.
