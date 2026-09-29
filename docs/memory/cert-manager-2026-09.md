---
name: cert-manager-2026-09
description: CertManagerInstallation slice, 2026-09-28 - fifth CRD, no provider enum, chart facts confirmed by live rendering (not yet run against a cluster)
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
  don't apply here). This is a stated assumption pending the runbook's step
  2 -- not yet live-verified against Talos's actual admission behavior.
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

**Verification status:** all Rust code (Tasks 1-6) built and the full test
suite passed in a scratch worktree, including the two `#[ignore]`d
real-chart tests in `helm.rs` against the live `quay.io` chart. **Nothing
has been run against an actual cluster yet** -- the namespace-admission
assumption above, the delete/cleanup behavior, and the self-signed
`ClusterIssuer`/`Certificate` smoke test are all still open per the
runbook.

**How to apply:** when bumping the chart version, re-render it
(`helm template cert-manager oci://quay.io/jetstack/charts/cert-manager
--version <new> --include-crds --no-hooks --namespace cert-manager --set
crds.enabled=true`) and re-check the CRD list, the Deployment/webhook
counts, and whether a `Namespace`/`Secret`/hook object starts appearing;
re-run the two ignored real-chart tests in `helm.rs`.
