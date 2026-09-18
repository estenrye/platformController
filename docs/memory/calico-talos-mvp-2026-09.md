---
name: calico-talos-mvp-2026-09
description: Status of the first platform-controller vertical slice (Calico-on-Talos install) as of 2026-09-18, and how it relates to the full goals.md vision
metadata:
  type: project
---

The full long-term vision for this repo is in `docs/goals.md`: a `CloudUnderlay` CRD supporting multiple cloud platforms and CNI providers. The first implemented slice is narrower: a `CniInstallation` CRD (group `platform.rye.ninja/v1alpha1`) that only handles `platformKind: talos-linux` + `provider: calico`, built via spec → plan → subagent-driven-development on 2026-09-18.

**Why scoped this way:** rather than building the full multi-cloud/multi-CNI schema up front, the MVP proves the reconcile pattern (Helm render → server-side apply → prune) on one concrete path first. See spec at `docs/superpowers/specs/2026-09-18-calico-talos-mvp-controller-design.md` and plan at `docs/superpowers/plans/2026-09-18-calico-talos-mvp-controller.md` for the full non-goals list (no airgapped chart vendoring, no fine-grained RBAC — see [[rbac-cluster-admin-tradeoff]] — no CNI-readiness polling, no leader election).

**Status:** implemented, reviewed, and live-verified against a real Talos-in-Docker cluster (Calico installed successfully, nodes reached Ready). Opened as PR #2 against `main` in `estenrye/platformController` on GitHub (repo created during this work — previously local-only). Implementation work happened in git worktree `worktree-calico-talos-mvp`.

**How to apply:** when asked to add a new component/CNI provider/platform to this controller, check whether the existing `CniInstallation` CRD should be extended or whether it's time to migrate toward the full `CloudUnderlay` schema from goals.md — the plan's fast-follow list explicitly anticipates "fold this CRD into the full CloudUnderlay schema once more provider/platform paths are implemented." Also check PR #2's merge status before assuming `main` reflects this work.
