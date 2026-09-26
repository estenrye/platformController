---
name: rbac-cluster-admin-tradeoff
description: The platform-controller's bootstrap ServiceAccount is bound to cluster-admin (must be revisited before production); tracks per-component permission requirements for a future scope-down
metadata:
  type: project
---

The `platform-controller`'s bootstrap manifest (`deploy/bootstrap.yaml`) binds the controller's ServiceAccount to the built-in `cluster-admin` ClusterRole, rather than a scoped custom role.

**Why:** the `tigera-operator` Helm chart (which installs Calico) creates its own ClusterRole with a long, version-dependent permission list (pods, CRDs, networkpolicies, webhooks, jobs, leases, etc.). Kubernetes' privilege-escalation prevention means the controller can only grant permissions it already holds. An initial hand-enumerated permission list was incomplete and caused reconcile to 403. The fix was ruled during [[calico-talos-mvp-2026-09]] implementation: bind to `cluster-admin` instead of maintaining an enumerated list, since (a) the project's spec already explicitly accepts "no fine-grained RBAC scoping" as an MVP trade-off deferred to a fast-follow, and (b) hand-enumerating permissions is fragile — it silently breaks again on the next Calico chart version bump that adds a new resource type.

**The trade-off is bigger than the spec anticipated.** The spec's "broad ClusterRole" language assumed something like "more permissions than strictly needed," not literal root access to the cluster. `cluster-admin` is the maximum privilege level available. This is acceptable for a sandboxed dev/test cluster but is a real production security exposure.

**How to apply:** flag this explicitly whenever asked to review, harden, or productionize this controller, or when building new components that share its ServiceAccount/RBAC. Before any real/production deployment, this needs to be scoped down to the actual permission set the controller and its managed components require (Calico's chart plus whatever future components get added) — likely as its own dedicated task once the MVP's set of managed components stabilizes, since scoping RBAC too early would mean re-deriving it on every new component added.

Documented in-repo at `deploy/bootstrap.yaml` (comment) and in PR #2's description; not yet acted upon as of this writing.

## Permission ledger (for scoping RBAC down later)

Running inventory of what each managed component actually needs, so a future scope-down task doesn't have to re-derive this from scratch. Update this table whenever a new component is added to the controller or a chart-version bump changes what it needs. Verbs are approximate (`get/list/watch/create/update/patch/delete` unless noted) — re-check exact verbs against the pinned chart version's rendered ClusterRole before actually cutting anything over, since chart bumps can add resources silently.

**`delete` is now load-bearing, not just latent.** The cleanup-on-delete work (finalizer-driven teardown on `CniInstallation` delete) means `delete` is exercised end-to-end on every kind the controller applies — Namespace, CRDs, RBAC objects, the operator Deployment, and provider-managed custom resources — where previously only create/patch/update were actually exercised in practice (the original ClusterRole listed `delete` defensively, but nothing called it). This doesn't change what's granted here (still `cluster-admin`); it's a note for whoever scopes this down later that `delete` can't be dropped or narrowed to a subset of kinds without breaking cleanup.

| apiGroup | Resources | Required by | Notes |
|---|---|---|---|
| `""` (core) | `namespaces`, `serviceaccounts`, `configmaps`, `secrets`, `services` | Controller itself | Original hand-rolled ClusterRole (pre-cluster-admin) already granted these |
| `""` (core) | `pods`, `podtemplates`, `endpoints`, `events`, `nodes`, `resourcequotas` | `tigera-operator` chart (Calico) | Found missing from the original ClusterRole during final review (2026-09-18); root cause of the 403 that motivated the cluster-admin switch |
| `apps` | `deployments`, `daemonsets` | Controller itself | Original ClusterRole |
| `apps` | `statefulsets`, `deployments/finalizers` | `tigera-operator` chart | Found missing during final review |
| `rbac.authorization.k8s.io` | `clusterroles`, `clusterrolebindings`, `roles`, `rolebindings` | Controller itself, to create the operator's own ClusterRole | Needs `escalate`/`bind` verbs too (privilege-escalation prevention) — this was the actual C1 defect, not just a missing resource type |
| `apiextensions.k8s.io` | `customresourcedefinitions` | Controller itself | Applies the tigera-operator CRDs from the chart's `--include-crds` output; also polls `get` on each applied CRD to wait for `Established` before continuing (added 2026-09-19, see [[wait-for-crd-established]]) |
| `admissionregistration.k8s.io` | `validatingwebhookconfigurations` | Controller itself | Original ClusterRole |
| `admissionregistration.k8s.io` | `mutatingwebhookconfigurations` | `tigera-operator` chart | Found missing during final review — original only granted *validating* |
| `apiregistration.k8s.io` | `apiservices` | Controller itself | Original ClusterRole |
| `operator.tigera.io` | `*` (wildcard) incl. `*/status`, `*/finalizers` | `tigera-operator` chart | Operator's own CRs (`Installation`, `APIServer`, etc.) |
| `crd.projectcalico.org` | `ippools`, `bgpconfigurations`, `bgppeers` (create/patch/delete, get/list via discovery) | Controller itself (added 2026-09-19, [[ipv6-only-calico-2026-09]]) | The controller now applies these directly: explicit pod `IPPool`s, LoadBalancer `IPPool`s, `BGPConfiguration`, `BGPPeer`. Also runs API discovery (`GET /apis/crd.projectcalico.org/v1`) to wait for the kind to be registered before applying. A scope-down that grants only create/patch will fail discovery silently as a timeout, not a 403. |
| `crd.projectcalico.org`, `projectcalico.org` | incl. `tier.networkpolicies`, `tiers` | `tigera-operator` chart | Found missing during final review |
| `networking.k8s.io` | `networkpolicies` | `tigera-operator` chart | Found missing during final review |
| `scheduling.k8s.io` | `priorityclasses` | `tigera-operator` chart | Found missing during final review |
| `policy` | `poddisruptionbudgets`, `podsecuritypolicies` | `tigera-operator` chart | Found missing during final review |
| `coordination.k8s.io` | `leases` | `tigera-operator` chart | Found missing during final review (likely leader-election support inside the operator itself) |
| `storage.k8s.io` | `csidrivers` | `tigera-operator` chart | Found missing during final review |
| `certificates.k8s.io` | `certificatesigningrequests` | `tigera-operator` chart | Found missing during final review |
| `batch` | `jobs` | *Not currently needed* | The chart's `pre-delete` hook Job would need this, but `--no-hooks` (added to fix C2 — see [[calico-talos-mvp-2026-09]]) means that Job is never rendered/applied. Only add this back if hook-based rendering is ever re-enabled. |
| `platform.rye.ninja` | `cniinstallations`, `cniinstallations/status` | Controller itself | Its own CRD — get/list/watch/update/patch (no delete needed, it doesn't self-manage) |
| `platform.rye.ninja` | `pullthroughcaches`, `pullthroughcaches/status` | Controller itself | Its own second CRD (2026-09-25) — get/list/watch/update/patch, same as `cniinstallations`; finalizer updates go through `update`/`patch` on the main resource. |
| `""` (core), `apps` | `serviceaccounts`, `services`, `daemonsets`, `namespaces` | Spegel chart (`0.7.4`, rendered with `--no-hooks`) | Adds no kinds beyond what the controller already applies for Calico. The chart renders no Role/ClusterRole. Its post-delete hook objects (DaemonSet, Pod, Service) are never applied while `--no-hooks` is on. |
| `coordination.k8s.io` | `leases` | Controller itself | Leader-election Lease (`platform-controller-leader` in `platform-system`, added 2026-09-18) — get/create/update only; no list/watch needed since the controller only ever touches its own single named Lease |
| `""` (core) | `events` (create) | *Not currently needed* | Would be required if Kubernetes `Event` emission (spec §4) is ever implemented — current observability is `tracing` log calls only, no `Recorder`/Event objects, so this permission is unused today |

**Pattern so far:** every row labeled "Controller itself" is stable (it's the controller's own CRD + the objects it needs to create for *any* Helm-chart-based component). Every row labeled "`tigera-operator` chart" is Calico-specific and was only discovered by actually rendering that chart and hitting real 403s — meaning **the same discovery process will likely be needed for every future component** (render its chart/manifests, diff against the current cluster-admin-shadowed permission set, add rows here). This table is the input to that future scope-down task, not a finished spec.
