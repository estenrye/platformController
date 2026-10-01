# EtcdEncryption (KMS-backed Secret encryption at rest) on Talos

Status: Implemented, NOT live-verified
Date: 2026-09-30

> **Caveat (read first).** The controller does **not** detect or merge a
> pre-existing encryption provider. Talos's default `talosctl gen config`
> already encrypts Secrets with secretbox (`cluster.secretboxEncryptionSecret`);
> every generated patch replaces the `providers` list, so the operator must
> splice the existing provider into each patch as a read fallback (every patch
> starts with a comment saying so). Forgetting makes every existing Secret
> unreadable. Key rotation is unsupported, and the feature has not been run
> against a live cluster. See "Known limitations" at the end.

## Purpose

Kubernetes stores Secrets in etcd as plaintext unless the apiserver is given
an `EncryptionConfiguration`. This spec adds a seventh platform component,
`EtcdEncryption`, that moves a Talos cluster to KMS-backed Secret encryption:
it installs the KMS plugin, re-encrypts every Secret that is plaintext in etcd
at install time, and walks the cluster to a state where plaintext storage is
disabled.

It is built in slices. **This slice ships the engine-agnostic core plus the
OpenStack Barbican provider** (the stack this controller already targets for
CCM and CSI). The API and internals are shaped so that Azure Key Vault, AWS
KMS, Google Cloud KMS and Oracle Cloud Infrastructure KMS can each be added as
a further `provider` value without reshaping the CRD or the phase logic. None
of those four is built here.

Upstream references: the Barbican plugin
(`kubernetes/cloud-provider-openstack`, `docs/barbican-kms-plugin`), the
Kubernetes KMS provider docs, and Talos's `KubeEtcdEncryptionConfig`
machine-config document.

## The constraint that shapes the design

On Talos, kube-apiserver is a static pod rendered from the **machine config**.
The `EncryptionConfiguration` is supplied through the `KubeEtcdEncryptionConfig`
document, applied through the **Talos API**, not the Kubernetes API. This
controller speaks only the Kubernetes API (the same stance as every earlier
component's "node prerequisite"), so it cannot change the apiserver's
configuration. That change must also happen in a specific order relative to
the plugin and the re-encryption, or the cluster breaks (apiserver unable to
read a Secret). So this component is a **protocol between the controller and
the operator**: the controller does everything it can reach through the
Kubernetes API, generates the exact Talos patches the operator must apply, and
verifies each step before advancing.

## Non-goals

- **Applying Talos machine config.** The controller never holds a Talos API
  credential. A later opt-in slice could add one; not designed here.
- **Creating, rotating or deleting KMS keys.** The key is created by the
  operator and referenced by ID. A controller bug that touched a key would be
  unrecoverable for the whole cluster's data.
- **Encrypting resources other than Secrets.** Only Secrets.
- **KMS-based Talos disk encryption** (STATE/EPHEMERAL partitions). Separate
  feature, machine-config only.
- **The Azure, AWS, GCP and OCI providers.** Designed for, not built.
- **Fetching the upstream plugin manifest.** The DaemonSet is generated in Rust
  (see Design), not fetched or patched from `master`-branch YAML.
- **A force-delete escape hatch** for a CR whose apiserver config still points
  at the plugin.
- **A shared component framework** across the now-seven reconcilers. Still the
  growing case it has been since the third CRD; still out of scope.
- **Detecting or merging a pre-existing encryption provider** (Talos's default
  secretbox, or any `KubeEtcdEncryptionConfig` already in the machine config).
  The controller cannot read the machine config; the operator splices it in.
- **Key rotation**, or changing `cloudConfigSecretRef` / the `key-id` after
  `Rewriting` has started: Secrets under the old key become unreadable.

## Design

### API

A new cluster-scoped singleton CRD in the existing group, following the
`provider` enum plus sibling-block shape `CloudControllerManager` uses:

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: EtcdEncryption          # cluster-scoped, shortname "etcdenc"
metadata:
  name: default               # singleton, same rule as the other six
spec:
  platformKind: talos-linux   # only value accepted today
  provider: barbican          # enum; azure, aws, gcp, oci come later
  barbican:
    image: registry.k8s.io/provider-os/barbican-kms-plugin:<tag>  # required, pinned, no default
    cloudConfigSecretRef:
      name: barbican-kms-cloud-config   # required; Secret in kube-system, key cloud.conf
  acknowledgements:
    kmsConfigApplied: false   # set true after applying Talos patch 1
    plaintextRemoved: false   # set true after applying Talos patch 2
    kmsReverted: false        # set true after applying the revert patch (deletion only)
    kmsRemoved: false         # set true after applying the remove-kms patch (deletion only)
```

- `image` is pinned explicitly with no default, the same convention as every
  `chartVersion`. The controller does not choose a plugin version for you.
- `cloudConfigSecretRef` names a Secret in `kube-system` holding OpenStack
  credentials plus the `[KeyManager] key-id` the plugin reads from
  `cloud.conf`. As with `CloudControllerManager`, the controller references the
  Secret by name only; it never reads its contents and nothing secret appears
  in the CR or its status. There is no `keyId` field: the plugin reads the key
  only from `cloud.conf`'s `[KeyManager] key-id`, which the controller never
  reads, so a copy on the CR could only contradict it.
- Only one provider block may be set, and it must match `provider`; otherwise
  a `Ready=False` condition with `reason: InvalidSpec`, like the other components.
- The four `acknowledgements` fields are the operator's explicit gates; they
  are the only fields the operator flips as the protocol progresses. They are
  always `false` on first apply.
- **An acknowledgement only counts if it was set after its patch was
  published.** `status.patchGenerations.{enableKms,removeIdentity,revert,removeKms}`
  records `metadata.generation` at the status write that first published each
  patch (set once, never overwritten). An ack counts only when the current
  generation is strictly greater. An ack already `true` when its patch
  appeared must be flipped `false` then `true` (any later spec change would
  do). This stops a pre-set `kmsReverted` from skipping the revert patch (the
  rewrite would then be a no-op and `removeKms` would strand every Secret)
  and a pre-set `plaintextRemoved` from reaching `Encrypted` without patch 2.
  One exception: for deletion engaged-ness the controller uses the RAW
  `kmsConfigApplied`, erring on the side of keeping the plugin.

### Provider interface (extensibility)

Each provider supplies a small builder with a fixed shape: validate its spec
block, render its plugin `DaemonSet` (and any supporting objects), name the
unix socket the apiserver must reach, and produce the KMS provider block of
the `EncryptionConfiguration`. Everything downstream — phases, probes, patch
generation, the Secret rewrite — is provider-independent. Adding a provider
means adding one builder and one enum variant. Provider differences expected
later: credentials (Azure client identity, AWS IAM role, GCP service account,
OCI instance principal) and key addressing (Azure key URL, AWS key ARN, GCP
key resource name, OCI key OCID).

### Plugin installation

The plugin `DaemonSet` is built in Rust from typed spec fields, not rendered
from a chart and not fetched from upstream. It is applied with the existing
server-side-apply path and mirrors the facts already established for other
host-level components:

- targets `node-role.kubernetes.io/control-plane` nodes, tolerates the control
  plane and `node.cloudprovider.kubernetes.io/uninitialized` taints;
- `hostNetwork: true` and `dnsPolicy: Default` (the repo-wide rule: no CNI or
  cluster-DNS dependence for anything on the bootstrap path);
- mounts the credentials Secret and a `hostPath` of `/var/lib/kms` so the unix
  socket (`/var/lib/kms/kms.sock` for Barbican) is visible to the apiserver
  static pod on the same node;
- lives in `kube-system`; no namespace is synthesized;
- a `readinessProbe` on the socket (`ls /kms/kms.sock`), so "Ready on every
  control-plane node" means the socket exists, and
  `priorityClassName: system-node-critical`.

### Phases

`status.phase` reports where the protocol is. The controller never moves a
phase backwards on its own and never un-acknowledges anything.

1. **InstallingPlugin.** Apply the DaemonSet. Advance when a Ready plugin pod
   exists on every control-plane node.
2. **AwaitingKmsConfig.** Publish `status.talosPatches.enableKms`: a ready-to-
   apply Talos patch containing the `KubeEtcdEncryptionConfig` with the KMS
   provider **first** and `identity` **second** (so existing plaintext stays
   readable), plus the `cluster.apiServer.extraVolumes` entry mounting
   `/var/lib/kms`. Only this patch carries the volume; the machine config
   keeps it, and repeating it would append a duplicate entry. Advance only when **both** the apiserver probe reports a
   KMS provider active **and** `kmsConfigApplied` is `true`. The plugin must
   already be Ready on every control-plane node before this patch is published,
   so a rolling apiserver restart can never find a missing socket.
3. **Rewriting.** Re-save every Secret so it is stored encrypted through KMS:
   page through all Secrets, write each back unchanged, retry on 409. The loop
   is idempotent and resumable across controller restarts; progress (total,
   rewritten, failed) is in status. Per-Secret errors (e.g. an admission
   webhook denying an update) are counted and retried. The phase cannot
   complete while any Secret is unrewritten. Only Secrets are rewritten.
4. **AwaitingPlaintextRemoval.** Publish `status.talosPatches.removeIdentity`:
   the Talos patch with `identity` removed. Advance only when
   `plaintextRemoved` counts, the probes pass, **and every Secret can be
   listed** through the apiserver (a list returns whole objects, so each must
   decrypt; any error blocks).
5. **Encrypted.** `Ready=True`. Unlike the other components, `Ready` here means
   the end state was verified, not just "manifests applied"; the status says
   what was verified and how (see Probes).

Deletion adds three phases: `RevertingKms`, `Decrypting`,
`AwaitingKmsRemoval`. There is no `Failed` phase: failures are a `Ready=False`
condition with a reason, so the protocol position survives them.

A probe going negative after a phase was reached sets a `Degraded` condition
and a reason; it does not regress the phase or reset an acknowledgement.

### Probes

The controller reads, through the Kubernetes API, whether the apiserver is
actually using KMS:

- the apiserver's `/metrics` endpoint exposes `apiserver_envelope_encryption_*`
  series that exist only when a KMS provider is configured; and
- a canary Secret written and read back through the API proves the round-trip
  works under the current configuration.

For phase 2 this is conclusive. For phase 4 it is not: etcd cannot be read
through the Kubernetes API, so "plaintext is no longer stored" is established
indirectly (KMS is the only provider reporting, the canary round-trips,
every Secret can be read, and `plaintextRemoved` is acknowledged) rather than
by inspecting etcd. The status says so. Every probe reaches one apiserver
behind the load balancer; per-node rollout is the operator's job (the runbook
waits for each node's apiserver pod to restart before the next node). This is why the acknowledgement flags are gates *in addition to* the
probes and not replaced by them.

### Deletion

The apiserver configuration lives in Talos, so the controller cannot undo it.
The finalizer has two modes:

- **Not engaged** (nothing can depend on the plugin): delete the DaemonSet
  and finish. Engaged-ness errs on the safe side:
  - `Pending` / `InstallingPlugin`: engaged unless the KMS probe positively
    reports no KMS provider (an unreadable probe counts as engaged; the
    operator may have applied a patch out of band);
  - `AwaitingKmsConfig`: engaged if the RAW `kmsConfigApplied` is set or the
    probe does not positively report no KMS provider;
  - every later phase: engaged.
- **Engaged**: the CR stays `Terminating` and the controller walks three
  steps, never returning success from the finalizer until the plugin is safe
  to remove. Every step but the last re-applies the plugin DaemonSet first.
  (1) `RevertingKms`: publish `status.talosPatches.revert`, a patch listing
  `identity` **first** and `kms` second, so new writes are plaintext while
  KMS can still read old ciphertext; wait for `kmsReverted` (counted only if
  set after the revert patch was published). (2) `Decrypting`: once the
  plugin is Ready on every control-plane node, rewrite every Secret; then
  `AwaitingKmsRemoval`: publish `status.talosPatches.removeKms` (identity
  only) and wait for `kmsRemoved` (counted likewise), a probe showing no KMS
  provider active, **and** every Secret listable. (3) Remove the DaemonSet.
  Going straight to `identity`-only would make every KMS-encrypted Secret
  unreadable. Waiting for the revert ack never moves the phase back from
  `Decrypting` or `AwaitingKmsRemoval`.

There is no force-delete in this slice.

### Failure handling

- Bad credentials Secret, missing key, unhealthy plugin: a `Ready=False`
  condition with a reason, retried like the other components. Phases never
  advance on failure and the phase never regresses.
- Validation failures (`platformKind`, provider/block mismatch, empty
  required fields) are a `Ready=False` condition with `reason: InvalidSpec`
  and are not retried until the spec changes. The phase is not changed and
  never regresses.
- Every networked poll uses `tokio::time::timeout_at`, never a bare await
  (see [[wait-for-crd-established]]).

### Security and RBAC

The controller already runs as `cluster-admin`
([[rbac-cluster-admin-tradeoff]]); this component adds a ledger row that
matters for the eventual scope-down: **`get`/`list`/`update` on every Secret
in the cluster** (the rewrite), `create`/`get`/`delete` on one canary Secret,
`get` on the apiserver `nonResourceURLs: /metrics`, and the usual
DaemonSet/apply verbs. The rewrite never logs Secret data; failures are
reported by namespace/name only.

### Testing

Mirrors the repo's existing pattern:

- Unit tests: the Barbican DaemonSet builder, the Talos patch generator, the
  phase state machine, and the rewrite loop against a fake client (pagination,
  409 retry, resumption, per-Secret failure).
- A schema example test over `examples/etcd-encryption.yaml`.
- An ignored integration test.
- A live runbook, `docs/runbooks/etcd-encryption-verification.md`, covering
  applying both patches on a real Talos cluster and verifying with the Talos
  doc's etcd-read check.
- A memory file under `docs/memory/` and the RBAC-ledger row above.

## Open verification items

Facts taken from upstream docs that this design does not assume and that the
live runbook must confirm before the slice is called verified:

1. The exact `apiserver_envelope_encryption_*` metric names and which of them
   are present under KMS v2 on the Kubernetes version Talos ships.
2. Whether Talos needs `cluster.apiServer.extraVolumes` (or another mechanism)
   to expose `/var/lib/kms` to the apiserver static pod, and the exact shape of
   the `KubeEtcdEncryptionConfig` KMS provider block on the Talos version in
   use. The Talos reference page documents only a secretbox example; KMS
   specifics come from the guides listed in the original request.
3. The Barbican plugin's image reference and tag.
4. Whether any admission webhook or controller in a typical cluster rejects a
   no-op `update` of a Secret (affects the rewrite's failure accounting).

## Known limitations

- **Pre-existing encryption providers** (Talos default secretbox) are neither
  detected nor merged; the operator splices them into every patch.
- **Key rotation is unsupported**; changing `cloudConfigSecretRef` or the
  `key-id` after `Rewriting` makes Secrets unreadable.
- **Adding a control-plane node after `Encrypted`** requires applying the same
  patches to it and a Ready plugin pod there before its apiserver serves.
- **HA partial rollouts** are not detected: probes reach one apiserver behind
  the load balancer; the runbook's per-node wait is the mitigation.
- **Status writes are merge patches without a resourceVersion** (I6 in the
  final review, not fixed in this slice).
- **Not live-verified.**
