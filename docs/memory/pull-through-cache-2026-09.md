---
name: pull-through-cache-2026-09
description: PullThroughCache (Spegel) slice, 2026-09-25 - second CRD next to CniInstallation; non-obvious chart facts and the live-verification outcome
metadata:
  type: project
---

`PullThroughCache` (cluster-scoped singleton `default`, shortname `ptc`) installs Spegel on Talos. Spec: `docs/superpowers/specs/2026-09-25-pull-through-cache-design.md`; plan: `docs/superpowers/plans/2026-09-25-pull-through-cache.md`; live acceptance: `docs/runbooks/pull-through-cache-verification.md`. It is a parallel component (own reconciler, second `Controller` in `main.rs`, same leader lease), not a refactor of the CNI reconciler; extract a shared component framework only when a third component shows what is genuinely shared. kube-fledged pre-warming, an `external` provider and an `aws-ecr` provider are deliberately not built.

**Non-obvious facts (from rendering the real chart, not assumed):**
- The OCI chart tag has **no `v` prefix** (`0.7.4`); `v0.7.4` is only the GitHub release tag and 404s on ghcr.io.
- `helm template` on an `oci://` chart prints `Pulled:`/`Digest:` lines to **stdout**; unstripped they parse as a bogus first manifest. `helm::strip_oci_pull_preamble` removes them; the ignored test `helm::tests::spegel_chart_renders_parseable_manifests_with_our_values` pins it.
- `registries` maps to `spegel.mirroredRegistries`, whose default `[]` means **every registry**, so omitting it mirrors private registries too. An explicitly empty list is rejected as ambiguous.
- Spegel's DaemonSet publishes on `hostPort` 30020, so it needs the CNI up first (Calico provides hostPort). The reconciler applies without waiting for that.
- Node cleanup on delete is a **post-delete Helm hook** (`templates/post-delete-hook.yaml`), and the controller renders with `--no-hooks` (required: rendered hooks would run as live objects at install), so it does not run on delete.
- Running before the CNI (host-network mode) was considered and **dropped**: the chart has no `hostNetwork` value and hard-codes `--bootstrap-kind=dns` against cluster DNS, so it needs a post-render DaemonSet patch plus the HTTP bootstrapper; only nodes after the first would benefit and the cache dies with the cluster. Apply `PullThroughCache` after the CNI is `Ready`.
- The Talos node prerequisite (`/etc/cri/conf.d/20-customization.part` with `discard_unpacked_layers = false`) cannot be applied by the controller and cannot be verified by it; `Ready` means manifests applied only.

**Live verification: not yet performed.** The runbook's steps 2-3 (peer-to-peer serving across two nodes, and whether containerd fails open to the upstream registry after the CR is deleted and the post-delete hook has not run) are unverified. Record the observed signal and the fail-open outcome here after running them.

**How to apply:** when bumping the Spegel chart version, re-render it (`helm template ... oci://...`) and re-check `spegel.mirroredRegistries`, `spegel.containerdRegistryConfigPath`, the hostPort, and whether the post-delete hook changed; run the ignored real-chart test.
