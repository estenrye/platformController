# Cert-Manager Installation on Talos

Status: Draft, awaiting review
Date: 2026-09-28

## Purpose

cert-manager is the de facto standard Kubernetes operator for issuing and renewing X.509 certificates (webhook TLS, ingress TLS, mTLS between in-cluster services). This spec adds a fifth platform component to the controller: a `CertManagerInstallation` CRD that installs the cert-manager Helm chart, following the exact shape already established by `CniInstallation`, `PullThroughCache`, `CloudControllerManager` and `CsiDriver`.

This spec installs cert-manager itself only. It does not configure any `ClusterIssuer`/`Issuer` (self-signed CA, ACME, per-cloud DNS-01 solvers) — that is a separate concern, deferred to a later component, the same way `CniInstallation` only installs Calico without owning IP-pool routing decisions downstream of it, and the pull-through-cache spec deferred its `external`/`aws-ecr` providers.

## Non-goals

- **Issuer/ClusterIssuer configuration.** No ACME, no self-signed CA bootstrap, no per-cloud DNS-01 solver bindings (Route53, Cloud DNS, Azure DNS, OCI DNS). A cluster operator configures these directly against the installed cert-manager once this component reports `Ready`, exactly as they would configure Calico IP routing details this controller doesn't own.
- **A `provider` enum.** Every other component's CRD carries one because it models a real per-cloud or per-implementation choice (Calico vs. a future Cilium; OpenStack vs. a future AWS EBS driver). There is no comparable alternative implementation for "the thing that installs cert-manager" today, so the spec is a flat `{ platformKind, chartVersion, helmValues }`, not a nested provider sub-struct.
- **trust-manager or any other Jetstack sibling project.** Only the `cert-manager` chart itself.
- **A health-derived `Ready` condition.** `Ready` means "manifests applied", exactly as it does for every existing component today — not "the webhook is serving" or "a test Certificate issued successfully".
- **Waiting for cert-manager's own CRDs** (`Certificate`, `Issuer`, `ClusterIssuer`, etc.) to be `Established` before doing anything else in the same reconcile. Nothing in this component creates a `cert-manager.io` resource, so there is no same-reconcile ordering problem the way there is for the Tigera operator's own CRDs during the CNI bootstrap.
- **Airgapped chart source.** The controller fetches from `oci://quay.io/jetstack/charts/cert-manager`, so it needs that egress (or a configured pull-through cache / registry mirror), same caveat as Spegel's `ghcr.io` dependency.

## Design

### API

A new cluster-scoped singleton CRD in the existing group, next to the other four:

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CertManagerInstallation   # cluster-scoped, shortname "certmgr" (not "cm": kubectl already
metadata:                       # treats that as shorthand for ConfigMaps)
  name: default                 # singleton, same rule as CniInstallation/CloudControllerManager/PullThroughCache
spec:
  platformKind: talos-linux     # only value accepted today
  chartVersion: "<pinned>"      # required, no default, same as every other chart
  helmValues: {}                # optional free-form passthrough
```

- `chartVersion` is the Helm chart version (cert-manager's chart and application versions track together, e.g. `v1.16.2`), passed verbatim to `helm template --version`.
- `helmValues` is merged first; the controller's own typed value (`crds.enabled: true`, see below) is overlaid afterwards, so a passthrough can never accidentally disable CRD installation.
- No `cleanupTimeoutSeconds`: cert-manager has no DaemonSet-shaped, node-resident resources analogous to Calico's `calico-node` that need a bounded wait on removal — deletion just prunes the ledger.

Validation, rejected with `phase: Failed` and a `reason`, same pattern as every other component:

- the name is not `default` (`Unsupported`)
- `platformKind` is unsupported (`Unsupported`)
- `chartVersion` is empty (`InvalidChartVersion`)
- `chartVersion` has leading or trailing whitespace (`InvalidChartVersion`)
- `helmValues` is not a JSON object (`InvalidHelmValues`)

Status mirrors every other kind and reuses `Phase`, `Condition` and `AppliedResourceRef`: `phase`, `observedGeneration`, `chartVersion`, `appliedResources`, `conditions`.

### Reconcile

Same shape as the pull-through-cache and CCM loops:

1. Leader gate, then validate; write `Failed` status on rejection.
2. Render the cert-manager chart via `helm::render_chart` with `ChartSource::Oci { reference: "oci://quay.io/jetstack/charts/cert-manager" }`, release `cert-manager`, namespace `cert-manager`. `--no-hooks` and `--include-crds` stay on, same as every chart.
3. Synthesize and apply a `cert-manager` namespace **without** `pod-security.kubernetes.io/*: privileged` labels. Unlike Calico (host-path CNI plugin binaries) and Spegel (containerd socket, host paths), cert-manager's `controller`, `webhook` and `cainjector` Deployments are plain, non-privileged, non-host-network pods — Talos's default `baseline` Pod Security Standard should admit them unmodified. This is a stated assumption, to be confirmed live (see the runbook, below) and corrected here if wrong.
4. `build_values` always sets `crds.enabled: true` (typed, controller-set, not user-controlled) — the same kind of unconditional platform-implied override as Calico's `flexVolumePath: None` — so the chart's own CRDs (rendered under its `crds/` directory, gated on this value in chart versions that support it) are always included regardless of what the pinned chart version's own default happens to be. `helmValues` is merged first, then this typed value is overlaid on top so a passthrough can never disable it.
5. Parse, sort and apply the rendered objects in rank order, then prune anything no longer rendered against `status.appliedResources`, same as every other component.
6. Write `Ready` status; requeue at 300s.

### Cleanup

The finalizer deletes applied resources in reverse ledger order (the namespace goes last), same generic prune-on-delete flow as every other component. No node-resident state and no post-delete Helm hook complications are expected (cert-manager ships none), so no special-cased fail-open behavior like Spegel's is anticipated — to be confirmed by the runbook.

### Wiring

- `main.rs` builds a fifth watcher and `Controller<CertManagerInstallation>` with the same generation/deletion-requested/finalizer predicate filter as every other loop, running in the existing `select!` alongside the leader task and SIGTERM handling, sharing the one `Context` and lease.
- The finalizer name (`platform.rye.ninja/cleanup`) is reused.
- `leader_gate` is reused from `reconciler.rs`, same as the other non-CNI reconcilers.

### Code layout

- `src/cert_manager.rs`: CRD types (`CertManagerInstallation`, `CertManagerInstallationSpec`, `CertManagerInstallationStatus`), validation, and the values builder.
- `src/cert_manager_reconciler.rs`: reconcile, cleanup, finalizer wiring, status — same shape as `cache_reconciler.rs`.
- `src/helm.rs`: new `CERT_MANAGER_NAMESPACE` const and `CERT_MANAGER_CHART: ChartRef` (OCI form, same as `SPEGEL_CHART`).
- `src/crds.rs`: `generated_yaml` gains the fifth CRD.
- `src/bin/crdgen.rs`: unchanged (it already delegates to `crds::generated_yaml`).

### Deployment and docs

- `deploy/crd.yaml` regenerated with all five CRDs; `deploy/README.md`'s `kubectl wait --for=condition=established` list gains `certmanagerinstallations.platform.rye.ninja`.
- `bootstrap.yaml` needs no RBAC change (cluster-admin already covers it).
- `examples/cert-manager.yaml`: a Talos starting point, `name: default`, a pinned `chartVersion`.
- New `deploy/README.md` section, "Cert-manager (optional)": apply **after** `CniInstallation` is `Ready` — cert-manager's pods run on the pod network and need cluster DNS the same way Spegel does — with no dependency on `CloudControllerManager` or `CsiDriver`. Update the shared "Apply order" note across sections to mention it.
- `docs/runbooks/cert-manager-verification.md`: live checks — namespace admission under `baseline` PSS (confirms or refutes the no-privileged-labels assumption above), a manual self-signed `ClusterIssuer` + `Certificate` issued successfully as a smoke test that the webhook and controller are actually functional (this is verification only; the CR shape for issuers is explicitly out of scope for this component itself), delete/cleanup behavior.
- A `docs/memory/` entry and index line, per `CLAUDE.md`.

## Testing

- **Unit:** spec deserialization and defaults; each validation rejection; values builder (`crds.enabled: true` always set, wins over a conflicting `helmValues` attempt, other passthrough keys survive the merge); render-argument builder for the OCI form (mirrors the existing `oci_render_args_pass_the_reference_without_a_repo_flag` test for Spegel).
- **Example:** the example manifest parses and validates, in the style of `tests/cloud_controller_manager_example.rs`.
- **Integration (ignored, Talos-in-Docker, in the style of `tests/integration_pull_through_cache.rs`):** apply the CR, assert the cert-manager `Deployment`s (controller, webhook, cainjector) appear and the namespace is admitted without a PSS violation, delete the CR, assert the applied resources and namespace are gone.
- **Live verification (manual, runbook):** the `baseline`-PSS-admission assumption above; a real certificate issuance smoke test (self-signed `ClusterIssuer` + `Certificate`, confirmed `Ready`) to prove the installed chart is actually functional beyond "manifests applied"; delete/cleanup behavior.
