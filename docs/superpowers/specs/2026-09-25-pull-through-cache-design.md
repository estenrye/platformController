# Pull-Through Image Cache (Spegel) on Talos

Status: Draft, awaiting review
Date: 2026-09-25

## Purpose

Nodes in a self-hosted Talos cluster pull every image from the upstream registry, so each node pays for the same pull, upstream rate limits (Docker Hub) bite during bootstrap and rebuilds, and an upstream outage blocks pod starts. This spec adds a second platform component to the controller: a **pull-through image cache**, first backed by [Spegel](https://spegel.dev), a node-to-node peer-to-peer registry mirror. Once one node has pulled an image, the others fetch its layers from that peer instead of upstream.

Spegel was chosen over an in-cluster registry-in-proxy-mode because it needs no storage, and this controller is designed to run before any CSI exists. It has no central component to keep available.

The API is shaped so that other cache backends can be added as further `provider` values without reshaping the CRD: an `external` provider (an existing mirror that outlives the cluster, e.g. Harbor) and an `aws-ecr` provider (configuring the platform's own pull-through cache instead of deploying anything). Neither is built here.

## Non-goals

- Pre-warming images on nodes (kube-fledged). Considered and deferred: it is a different problem (availability of a chosen image set) from pull-through caching.
- The `external` and `aws-ecr` providers.
- Applying Talos machine config from the controller. The controller speaks only the Kubernetes API; node configuration is a documented prerequisite (below). Owning node config would need Talos API credentials, a Talos gRPC client and an `os:admin`-class trust boundary, and can trigger reboots. That is its own project.
- A health-derived status condition. `Ready` means "manifests applied", exactly as it does for `CniInstallation` today.
- An airgapped chart source. The controller fetches the Spegel chart from `ghcr.io`, so it needs that egress.
- Running Spegel before the CNI (host-network mode), so it could serve Calico's own images. Considered and dropped. Spegel `0.7.4` is only usable after the CNI is up: the chart exposes no `hostNetwork` value and hard-codes `--bootstrap-kind=dns` against a cluster-DNS name (CoreDNS cannot run before a CNI). Making it work would mean patching the rendered DaemonSet after rendering (host networking, plus the HTTP bootstrapper pointed at a chosen node), and even then only nodes after the first would benefit, with the cache lost on every cluster rebuild. Apply `PullThroughCache` after `CniInstallation` is `Ready`.
- Refactoring the CNI reconciler into a shared component framework. Revisit when a third component shows what is genuinely shared.

## Node prerequisite (documented, not automated)

Spegel on Talos needs a one-time machine-config change that this controller cannot make. Per Spegel's Talos documentation, every node needs:

```yaml
machine:
  files:
    - path: /etc/cri/conf.d/20-customization.part
      op: create
      content: |
        [plugins."io.containerd.cri.v1.images"]
          discard_unpacked_layers = false
```

Without it containerd discards unpacked layers and there is nothing for peers to serve. Spegel then writes its own per-registry mirror config under `/etc/cri/conf.d/hosts`. The exact patch is recorded in `docs/runbooks/pull-through-cache-verification.md` and verified against the Talos version in use. The controller cannot verify the patch is applied; it reports only that its own manifests were applied.

## Design

### API

A new cluster-scoped singleton CRD in the existing group, next to `CniInstallation`:

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: PullThroughCache        # cluster-scoped, shortname "ptc"
metadata:
  name: default               # singleton, same rule as CniInstallation
spec:
  platformKind: talos-linux   # only value accepted today
  provider: spegel            # enum; "external" and "aws-ecr" come later
  spegel:
    chartVersion: "<pinned>"  # required, no default, same as Calico
    registries: [docker.io, ghcr.io]   # optional
    helmValues: {}                     # optional free-form passthrough
```

- `registries` maps to the chart's `spegel.mirroredRegistries`. **Omitting it leaves the chart default, `[]`, which means every registry is mirrored** (private registries included). Setting it restricts mirroring to exactly those registries. The example manifest sets it explicitly so the choice is visible.
- `helmValues` is merged first; typed fields and the controller's own Talos setting are overlaid afterwards, so a passthrough can never silently contradict a typed field.
- For `talos-linux` the controller always sets `spegel.containerdRegistryConfigPath` to `/etc/cri/conf.d/hosts`, the same kind of unconditional platform-implied value as Calico's `flexVolumePath: None`.
- No `cleanupTimeoutSeconds`: Spegel has no operator-owned resources that need a bounded wait on removal.

Validation, rejected with `phase: Failed` and a `reason`, like the CNI path:

- the name is not `default`
- `platformKind` or `provider` is unsupported
- `chartVersion` is empty
- a `registries` entry is not a bare hostname (no scheme, no path, non-empty)
- `helmValues` is not a JSON object

Status mirrors `CniInstallationStatus` and reuses `Phase`, `Condition` and `AppliedResourceRef`: `phase`, `observedGeneration`, `chartVersion`, `appliedResources`, `conditions`.

### Reconcile

Same shape as the CNI loop:

1. Leader gate, then validate; write `Failed` status on rejection.
2. Render the Spegel chart. `helm::render` is generalized to take a chart source, namespace and values. Spegel is published as an OCI artifact (`oci://ghcr.io/spegel-org/helm-charts/spegel`) while Calico uses `--repo`, so the render-argument builder must produce both invocation forms. `--no-hooks` and `--include-crds` stay on.
3. Synthesize and apply a `spegel` namespace labelled `pod-security.kubernetes.io/{enforce,audit,warn}: privileged`. Spegel mounts the containerd socket and host paths, which Talos's default `baseline` policy rejects.
4. Parse, sort and apply the rendered objects in rank order (the existing wait-for-kind handling covers any chart custom resources), then prune anything no longer rendered against `status.appliedResources`.
5. Write `Ready` status; requeue at 300s.

### Cleanup

The finalizer deletes applied resources in reverse ledger order (the namespace goes last). There are no removal waits.

**Open item.** Spegel removes the mirror config it wrote on each node via a post-delete Helm hook. Rendering with `--no-hooks` is required (a rendered hook Job would be applied as a live object and run at install), so that hook will not run on delete. After the DaemonSet is gone, nodes keep their Spegel-written mirror config. Whether containerd then fails open to the upstream registry, or stalls pulls against a dead local mirror, must be established on a live cluster before implementation is considered done. If it does not fail open, cleanup needs an explicit node-cleanup step (or a controller-run equivalent of the hook) and this section changes.

### Wiring

- `main.rs` builds a second watcher and `Controller<PullThroughCache>` with the same generation, deletion-requested and finalizer predicate filter. Both controllers run in the existing `select!` alongside the leader task and SIGTERM handling, sharing one `Context` and one lease.
- The finalizer name (`platform.rye.ninja/cleanup`) is reused: finalizers are per object.
- Finalizer, status and leader-gate glue (roughly 150 lines) is duplicated from `reconciler.rs` rather than extracted. The CNI path, live-verified after several cleanup fixes, is untouched apart from `helm::render`.

### Code layout

- `src/pull_through_cache.rs`: CRD types and the Spegel values builder.
- `src/cache_reconciler.rs`: reconcile, cleanup, finalizer wiring, status.
- `src/helm.rs`: `render` and `build_render_args` generalized (chart source, namespace, values). Existing CNI tests must pass unchanged.
- `src/bin/crdgen.rs`: emits both CRDs.

### Deployment and docs

- `deploy/crd.yaml` regenerated with both CRDs; `deploy/README.md` waits for `established` on both before applying `bootstrap.yaml` or any custom resource.
- `bootstrap.yaml` needs no RBAC change (cluster-admin). The RBAC ledger memory gains a Spegel row.
- `examples/pull-through-cache.yaml`: a Talos starting point.
- `docs/runbooks/pull-through-cache-verification.md`: the machine-config prerequisite, and the live check (pull an image on one node, confirm a second node fetches it from its peer).
- A `docs/memory/` entry and index line, per CLAUDE.md.

## Testing

- **Unit:** spec deserialization and defaults; each validation rejection; values builder (typed fields win over `helmValues`, Talos config path always set, omitted `registries` leaves `mirroredRegistries` unset); render-argument builder for both the OCI and `--repo` forms.
- **Example:** the example manifest parses and validates, in the style of `tests/ipv6_example.rs`.
- **Integration (ignored, Talos-in-Docker, in the style of `tests/integration_talos.rs`):** apply the CR, assert the Spegel DaemonSet appears, delete the CR, assert the applied resources are gone. It does not try to prove peer-to-peer serving.
- **Live verification (manual, runbook):** peer-to-peer serving across two nodes, and the delete/fail-open behaviour from the open item above.

## Unverified details to confirm during implementation

These come from Spegel's public docs and chart values, read while writing this spec, not from a running cluster:

- The exact chart value names (`spegel.mirroredRegistries`, `spegel.containerdRegistryConfigPath`) against the pinned chart version.
- That the Talos machine-config patch above is sufficient on the Talos version in use.
- Whether Spegel needs a working pod network (the controller runs before the CNI; applying manifests does not depend on it, but the DaemonSet's behaviour before the CNI is up should be observed).
- The post-delete fail-open behaviour described above.
