# MVP: Calico-on-Talos Installer Controller

Status: Approved for planning
Date: 2026-09-18

## Purpose

Ship the first working vertical slice of the platform operator described in
[docs/goals.md](../../goals.md): a Rust controller that, installed on a Talos
Linux Kubernetes cluster with no CNI, deploys Calico and brings the cluster to
a working networking state — entirely declaratively, via a Kubernetes custom
resource.

This is intentionally narrow. The full `CloudUnderlay` CRD in goals.md
(multi-cloud profiles, six CNI providers, generic Helm/ImageTag/chart
plumbing) is the long-term target; this spec covers only the first path
through it (`self-hosted-talos` platform, `calico` CNI provider), built as its
own small CRD that can be folded into the larger schema later without an API
break, since field names/semantics are kept aligned with goals.md's
`CalicoBinding`.

## Non-goals

- Any `platformKind` other than `talos-linux`, or any CNI provider other than
  `calico`.
- Airgapped operation — the Calico Helm chart is always fetched live from
  Tigera's public Helm repository at reconcile time.
- Fine-grained RBAC scoping for the controller (a broad `ClusterRole` is
  accepted for the MVP; least-privilege scoping is a follow-up once the exact
  set of rendered object kinds is proven stable).
- Gating `status.phase = Ready` on actual CNI/node readiness (Calico pods
  running, nodes `Ready`). MVP reports `Ready` once manifests are
  successfully applied; true readiness polling is a fast-follow (see
  Section 5).
- Multi-replica/leader-election high availability for the controller.
- Any CRD/controller upgrade or migration tooling.
- A packaged installer/CLI — bootstrapping is a single manifest applied by
  hand via `kubectl apply -f`.

## 1. Custom Resource: `CniInstallation`

Group: `platform.rye.ninja`, version `v1alpha1`, kind `CniInstallation`,
cluster-scoped. For the MVP only a single resource named `default` is
reconciled (other names are accepted by the API server but ignored/rejected
by the controller with a `Failed`/`Unsupported` condition, so the CRD schema
doesn't need a validating webhook to enforce singleton-ness in v1).

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CniInstallation
metadata:
  name: default
spec:
  platformKind: talos-linux      # enum, only "talos-linux" valid in MVP
  provider: calico                # enum, only "calico" valid in MVP
  calico:
    chartVersion: "v3.29.1"       # required, pins the tigera-operator chart version
    bgpEnabled: false
    apiServerEnabled: false
    ipPools:
      - name: default
        cidr: "10.244.0.0/16"
        encapsulation: None        # IPIP | VXLAN | None
        natOutgoing: true
        blockSize: 112
        nodeSelector: "all()"
    nodeAddressAutodetectionV6Cidrs: []   # calicoNetwork-level, NOT per-pool
                                            # (deviation from goals.md's per-pool
                                            # placement — corrected to match the
                                            # real tigera-operator Installation API)
status:
  phase: Pending | Installing | Ready | Failed
  observedGeneration: 0
  chartVersion: ""                 # last successfully applied chart version
  appliedResources:                 # kind/namespace/name of every object applied,
    - apiVersion: apps/v1           # used to compute prune candidates on re-render
      kind: Deployment
      namespace: tigera-operator
      name: tigera-operator
  conditions:
    - type: Applied
      status: "True"
      reason: ManifestsApplied
      lastTransitionTime: "..."
```

Field notes:
- `bgpEnabled`, `natOutgoing` are booleans in the CRD (clean API) but the
  underlying `tigera-operator` chart/`Installation` CR represents these as
  string enums (`Enabled`/`Disabled`). The controller translates when
  building Helm values (Section 2).
- `ipPools`, `bgpEnabled`, `apiServerEnabled` map directly to the
  `calicoNetwork.ipPools`, `calicoNetwork.bgp`, and `apiServer.enabled` keys
  of the chart's `values.yaml`, matching field names already defined in
  goals.md's `CalicoBinding`/`CalicoIpPoolSpec` (with the one correction
  noted above).

## 2. Reconcile Flow

Built on `kube-rs`'s `Controller` runtime, watching `CniInstallation`.

1. **Validate.** If `spec.platformKind != talos-linux` or
   `spec.provider != calico`, set `status.phase = Failed`, condition
   `type: Unsupported`, and requeue slowly (e.g. 10 min) without further
   action.
2. **Build Helm values.** Translate `spec.calico.*` into the shape the
   `tigera-operator` chart expects:
   ```yaml
   installation:
     enabled: true
     calicoNetwork:
       bgp: Enabled|Disabled                 # from bgpEnabled
       ipPools: [...]                         # natOutgoing/bgp-style bools -> enums
       nodeAddressAutodetectionV6:
         cidrs: [...]
   apiServer:
     enabled: true|false                      # from apiServerEnabled
   ```
   Serialize to a temp file (`tokio::fs`, cleaned up after render).
3. **Render.** Shell out to the `helm` binary (bundled in the controller's
   container image):
   ```
   helm template calico \
     --repo https://projectcalico.docs.tigera.io/charts tigera-operator \
     --version <spec.calico.chartVersion> \
     --values <tmpfile> \
     --include-crds
   ```
   No `helm repo add` — invoking with `--repo` inline keeps the controller
   stateless and avoids concurrent-reconcile races over a shared Helm repo
   cache. Non-zero exit or stderr output surfaces as a reconcile error
   (`status.phase = Failed`, condition `type: RenderFailed`) with exponential
   backoff requeue.
4. **Parse.** Split the rendered multi-document YAML into
   `kube::api::DynamicObject`s via `serde_yaml`'s multi-doc `Deserializer`.
5. **Apply, in dependency order**, via Kubernetes server-side apply
   (`PatchParams::apply("platform-controller")`):
   1. `Namespace`
   2. `CustomResourceDefinition`
   3. RBAC (`ServiceAccount`, `ClusterRole`, `ClusterRoleBinding`)
   4. Workloads (`Deployment`)
   5. The operator's own custom resources (`Installation`, `APIServer`) last,
      since they depend on CRDs from step 2 already being established.
6. **Prune.** Compare the newly rendered object set against
   `status.appliedResources` from the previous reconcile; delete any object
   present in the old set but absent from the new one (handles chart version
   bumps that remove resources). Objects are matched by
   `(apiVersion, kind, namespace, name)`.
7. **Update status.** `phase = Ready`, `observedGeneration`, `chartVersion`,
   `appliedResources`, condition `type: Applied, status: "True"`.
8. **Requeue.** On error: exponential backoff via `Action::requeue`. On
   success: periodic resync (every 5 minutes) so drift is self-corrected even
   without a spec change, independent of Kubernetes watch events.

## 3. Bootstrapping the Controller onto a No-CNI Cluster

The controller must run before any pod networking exists, which constrains
its own deployment manifest:

- `hostNetwork: true`, `dnsPolicy: ClusterFirstWithHostNet` — without this the
  kubelet cannot construct a pod sandbox at all (no CNI plugin available to
  invoke), and the pod would hang in `ContainerCreating` indefinitely.
- Tolerations for `node.kubernetes.io/not-ready` (`NoSchedule` and
  `NoExecute`) — a node with no working CNI reports `Ready=False` and
  Kubernetes auto-taints it, which would otherwise block scheduling.
- Toleration + `nodeSelector`/affinity for the control-plane taint/label, so
  the controller lands on control-plane nodes, which are guaranteed to exist
  and run the API server on a freshly bootstrapped Talos cluster.
- `replicas: 1` — no leader election in the MVP (explicit non-goal).
- Container image bundles the controller binary and the `helm` CLI binary.

**RBAC:** a `ClusterRole` covering every object kind the `tigera-operator`
chart renders (`Namespace`, `CustomResourceDefinition`, `ServiceAccount`,
`ClusterRole`/`ClusterRoleBinding`, `Deployment`, `ConfigMap`, `Secret`,
`Service`, `ValidatingWebhookConfiguration`, `APIService`, plus the
operator's own CRs) — necessarily broad, called out as a known MVP trade-off
rather than hidden.

**Install artifact:** one bootstrap manifest (CRD definition, `Namespace`,
`ServiceAccount`/RBAC, controller `Deployment`, and a default
`CniInstallation` resource) applied by hand via `kubectl apply -f
bootstrap.yaml` immediately after `talosctl bootstrap`, before any other
cluster interaction.

## 4. Observability

- `tracing` spans/events around each reconcile phase (validate, render,
  apply, prune), already scaffolded in [src/main.rs](../../../src/main.rs).
- Kubernetes `Event` objects emitted on phase transitions and errors
  (`ManifestsApplied`, `RenderFailed`, `ApplyFailed`), visible via
  `kubectl describe cniinstallation default`.
- `status.conditions` as the durable, machine-readable record of outcome.

## 5. Fast-Follows (explicitly out of scope now, tracked for later)

- Poll `calico-node` DaemonSet status / node `Ready` conditions before
  flipping `status.phase` to `Ready`, rather than treating "applied" as
  "ready."
- Vendor the Calico chart into the controller image for airgapped operation.
- Scope the bootstrap `ClusterRole` down to the minimal verified resource
  list.
- Fold this CRD into the full `CloudUnderlay` schema from goals.md once more
  provider/platform paths are implemented.
- Leader election for multi-replica controller HA.

## 6. Testing Strategy

- **Unit tests** (no cluster required): CRD-spec → Helm-values translation
  (including bool→enum mapping), multi-document YAML parsing, apply-ordering
  logic, and prune-diff computation.
- **Integration test**: end-to-end against `talosctl cluster create`
  (Talos-in-Docker, started with no CNI) — apply the bootstrap manifest and a
  `CniInstallation`, assert nodes reach `Ready` and `calico-node` pods come
  up. This is the real proof the MVP works end-to-end; run as an opt-in or
  nightly CI job rather than on every commit, since booting Talos nodes is
  slow.
- No `envtest`-equivalent exists in the `kube-rs` ecosystem (unlike
  controller-runtime in Go), so reconcile-loop correctness is exercised via
  the integration test above rather than a fake API server.
