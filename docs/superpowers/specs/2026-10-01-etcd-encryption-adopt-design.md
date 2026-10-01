# EtcdEncryption: adopt an existing KMS setup (observe, migrate, verify)

Status: Implemented, NOT live-verified
Date: 2026-10-01
Supersedes: `2026-09-30-etcd-encryption-design.md` (the fresh-install flow shipped in v0.1.11)

## Purpose

`EtcdEncryption` v0.1.11 assumed a cluster that starts with plaintext Secrets and
had the controller install a KMS plugin DaemonSet, publish Talos patches and walk
an acknowledgement protocol. Running it against a real Talos cluster (2026-10-01)
showed that premise is wrong for the clusters this controller is for:

- **The plugin already ran as Talos static pods** (`barbican-kms-plugin-<node>`),
  sharing `/var/lib/kms/kms.sock`. A DaemonSet is also the wrong shape: its pods
  mount a credentials Secret that is itself KMS-encrypted, so after a control-plane
  cold start nothing can decrypt that Secret and the plugin can never start. Static
  pods read their config from the node's disk and have no such cycle.
- **KMS was already configured, with Talos's default secretbox kept as a read
  fallback.** The apiserver's own metrics showed it:

  ```
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="to_storage",  transformer_prefix="k8s:enc:kms:v2:barbican:"} 1
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 284
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:secretbox:v1:"} 86
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="key2:"} 86
  ```

  New writes already went through KMS; 86 Secrets were still stored under secretbox.
  The v0.1.11 "enable KMS" patch would have dropped secretbox and locked those out.
- **The v0.1.11 probe could not tell whether KMS was in use.** The
  `apiserver_envelope_encryption_*` family has a bare gauge
  (`..._dek_cache_fill_percent 0`) whenever a provider is *configured*.

So this controller's useful job on such a cluster is the part that is hard to do by
hand and easy to get wrong: **recognise what is configured, rewrite every Secret
through the KMS provider, and prove, per apiserver, that nothing is left under a
legacy provider**, before telling the operator it is safe to drop that provider.

## Non-goals

- **Installing, running or managing the KMS plugin.** Operator-provided (static pods).
  A later slice may generate the Talos static-pod patch.
- **Enabling KMS in the apiserver, or editing the EncryptionConfiguration.** The
  controller publishes no Talos patch. Only the operator knows their existing config.
- **Creating, rotating or deleting KMS keys.** Unchanged.
- **Encrypting resources other than Secrets.**
- **Proving the legacy provider is gone from the config.** The apiserver's metrics
  cannot show that; `acknowledgements.legacyProvidersRemoved` is the operator's
  statement and the status says so.
- **Providers other than the configured KMS provider's name.** Azure, AWS, GCP and
  OCI differ only in the plugin the operator runs; this controller observes the
  apiserver, so it is provider-agnostic by construction. `kmsProviderName` is
  a string, not an enum.
- **A configurable apiserver port.** Fixed at 6443 (the Talos default).

## Design

### API (a breaking change to the `v1alpha1` CRD)

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: EtcdEncryption        # cluster-scoped singleton "default", shortname etcdenc
metadata: {name: default}
spec:
  platformKind: talos-linux
  kmsProviderName: barbican   # the `name` of the kms entry in your EncryptionConfiguration;
                              # target storage prefix is k8s:enc:kms:v2:<name>:
  rewrite: Disabled           # Disabled (default: observe only) | Enabled
  acknowledgements:
    legacyProvidersRemoved: false   # you set true after removing the legacy providers
```

- `status.phase` is read leniently: any value this version does not know (every
  v0.1.11 phase, such as `AwaitingKmsConfig`), `null` or another shape becomes
  `Observing`, so a leftover v0.1.11 object deserializes, its finalizer can be stripped,
  and the phase is re-derived on the next reconcile. Serialization and the CRD schema
  are unchanged. Other old status fields (`appliedResources`, `talosPatches`,
  `patchGenerations`) are ignored; old `rewrite` (`u64` counters) still fits.
- `kmsProviderName` is required: non-empty, no whitespace, no `:`. It is deserialized
  with a default of `""` and rejected at validation (`InvalidSpec`), so a CR left over
  from v0.1.11 (which has no such field) is reported instead of breaking the watcher.
- Everything the apiserver reports under a **top-level** prefix other than the target
  is **legacy**. Top-level prefixes are those beginning `k8s:enc:`; the inner
  `key2:` seen above is a per-key sub-prefix of the same secretbox operation and is
  ignored so reads are not double-counted. Reads reported with an empty prefix (what
  identity/plaintext storage presumably reports; not yet observed) are also legacy.
  There is no `legacyProviders` field to keep in sync.
- Removed from v0.1.11: `provider`, `barbican` (`image`, `cloudConfigSecretRef`),
  and the acknowledgements `kmsConfigApplied`, `plaintextRemoved`, `kmsReverted`,
  `kmsRemoved`.

### Status

```yaml
status:
  phase: Observing | NotConfigured | Migrating | ReadyToRemoveLegacy | Verified
  observedGeneration: <i64>
  legacyPrefixes: [k8s:enc:secretbox:v1:]
  nodes:
  - name: talos-controlplane-1
    address: 10.0.0.11
    verified: true
    writerPrefix: k8s:enc:kms:v2:barbican:
    readsByPrefix: {"k8s:enc:kms:v2:barbican:": 370, "k8s:enc:secretbox:v1:": 0}
    secretsListed: 370
    reason: ""
  rewrite: {total: 0, rewritten: 0, failed: 0}     # all i64 (u64 emits a CRD format warning)
  conditions: [Ready]                              # Ready=True only when Verified
```

All counters are `i64`.

### Phase is derived, not remembered

Each reconcile re-derives the phase from current evidence plus the spec. There is no
persisted protocol position, no acknowledgement-after-publication mechanism, no
ledger and no `patchGenerations`: the controller publishes nothing for an operator to
apply, so there is nothing to order. (The v0.1.11 final review's open Critical, minor
bugs and wording problems all lived in the removed machinery.)

The derivation, over the per-node results below:

1. No control-plane node discoverable, or **any node unverifiable** → `Observing`,
   `Ready=False`, the reason naming each node.
2. Every node verifiable and **no node's writer is the target provider** and none
   reports `kms-providers` → `NotConfigured` (message: this controller does not enable
   KMS; see the runbook).
3. Writers differ across nodes (a rolling apiserver restart) → `Observing`
   (`mixed`), never `Migrating`.
4. **Every node's writer is the target provider:**
   - the reader check is complete on every node with **zero legacy reads** →
     `ReadyToRemoveLegacy`, or `Verified` if `legacyProvidersRemoved` is true;
   - otherwise, if at least one node's reader check is complete and shows legacy reads > 0,
     and `rewrite: Enabled` → `Migrating` (run one rewrite pass); if no node's reader check
     is complete with legacy reads and the cluster is not all-clean, the phase is `Observing`:
     with reason "cannot verify reads on every apiserver; not rewriting" (when no reader is
     complete with legacy reads) or "legacy objects remain; set `rewrite: Enabled` to migrate"
     (when legacy reads were seen but rewrite is Disabled).

`Verified` also requires the canary Secret to round-trip. It means "the probes agree
and you acknowledged removal", and says it cannot prove the config no longer lists a
legacy provider.

### Per-apiserver verification

The controller discovers the control-plane apiservers from the Nodes labelled
`node-role.kubernetes.io/control-plane` (their `InternalIP`, port 6443) and builds a
client per node from the in-cluster config, with `cluster_url` overridden and the TLS
server name set to `kubernetes.default.svc` (the apiserver's serving certificate
probably does not list node IPs; this is unverified and a failure is simply "cannot
verify" for that node).

Discovery must not silently miss an apiserver:

- **No InternalIP:** a control-plane-labelled node without an `InternalIP` is kept
  with an empty address and is `Unverifiable` ("node has no InternalIP") without any
  network call.
- **Endpoint cross-check:** after verifying the discovered nodes, the controller lists
  the `default/kubernetes` EndpointSlices (label `kubernetes.io/service-name=kubernetes`)
  and, for every endpoint address that matches no discovered node's address (compared
  as IP addresses when both parse, so `fd00::11` equals `fd00:0:0:0:0:0:0:11`; string
  equality otherwise), adds a synthetic node `endpoint <address>` that is `Unverifiable`
  ("an apiserver registered in the kubernetes Endpoints is not a discovered
  control-plane node"). A failure to list the EndpointSlices is an error, like a
  discovery failure, never "nothing to check".

Per node, bounded by `timeout_at`:

1. **Readiness:** `GET /readyz?verbose`; record whether a `kms-providers` check is present and ok.
2. **Writer check:** snapshot `apiserver_storage_transformation_operations_total`
   (`resource="secrets"`), write the canary Secret through this node, snapshot again.
   The `to_storage` delta by top-level prefix says which provider this apiserver
   *writes* with: writer is the target only if the target prefix rose and no other
   top-level prefix did. A negative delta (counter reset by an apiserver restart
   mid-check) is "cannot verify".
3. **Reader check:** snapshot, **read every Secret from etcd**, snapshot. The Secret
   names are paged with a `limit` (on Kubernetes >= 1.33 such a list may be served
   from the watch cache, which decrypts nothing, so it supplies names only), then
   each Secret is fetched with a **GET with no `resourceVersion`**, which the
   apiserver's cacher serves from storage, so each Secret is decrypted under its
   stored prefix. The object is dropped at once (never logged or returned). A 404
   (deleted since listed) is skipped and not counted; any other GET error (including
   a 500 from a decryption failure) makes the reader check fail (`reader_error`),
   never clean. Each GET is bounded; the whole read is bounded at 15 minutes. The
   `from_storage` delta by top-level prefix is `readsByPrefix`. The check is
   *complete* only if the deltas sum to at least the number of Secrets read (every
   object was decrypted) — otherwise "cannot verify".
4. **Restart detection:** every `/metrics` snapshot also records the
   `process_start_time_seconds` gauge. The two writer snapshots must carry the same,
   present value, else the writer is "cannot verify" ("the apiserver restarted (or
   exposes no process_start_time_seconds) during the check"); the two reader
   snapshots must too, **and** must equal the writer's, else the reader check fails.
   This catches a restart whose counters only rose after it (a negative delta alone
   would miss it).

Concurrent readers only inflate deltas, so the check can wrongly look *worse*
(extra legacy reads) but not wrongly *clean*: zero legacy reads with a complete count
is the proof.

**Residual risk:** The completeness check compares the sum of decrypts against the number
of Secrets read; ambient Secret reads by other clients on the same apiserver during
the observation window can inflate the decryption count. The check is evidence, not proof.
However, the reader does not rely on a list: it GETs each Secret with no
`resourceVersion`, which the apiserver serves from etcd, so every Secret is itself
decrypted under its stored prefix and a legacy-stored Secret always adds a legacy read
(ambient reads can only add, never mask one). That an uncached GET bypasses the watch
cache is from the apiserver's cacher code and is unverified live. The runbook must
confirm it on a quiet cluster, per apiserver (per-Secret GETs raise `from_storage` by at
least the number read), and direct the operator to confirm with an etcd snapshot that
the legacy provider can be dropped before removing it from the EncryptionConfiguration.

**Scope:** the evidence covers Secrets only. The `ReadyToRemoveLegacy`/`Verified`
reasons say "no SECRET is stored under a legacy provider"; the runbook's removal gate
makes the operator check that no other resource in the EncryptionConfiguration uses
the legacy provider.

The reader check GETs every Secret on every control-plane node each time it runs. It
is the expensive step, so a steady-state reconcile (`ReadyToRemoveLegacy`, `Verified`,
`NotConfigured`) requeues every 10 minutes and an active one (`Observing`,
`Migrating`) every 30 seconds.

### Rewrite

Runs only with `rewrite: Enabled`, and only in the `Migrating` derivation, i.e. only
after the writer check showed the target provider on **every** node. It reuses the
existing paged rewrite loop (`secret_rewrite.rs`) unchanged: `replace` each Secret
unchanged (a 409 retried, a 404 counted done, any other per-Secret error counted
failed and reported by name, never by data). Once every apiserver writes through the
target provider, which apiserver the load balancer picks no longer matters. Progress
is written to `status.rewrite` per page. A restart simply repeats the pass.

### Failure handling

- A node that cannot be reached, answers with an error, or fails its checks is
  `verified: false` with a `reason`; the cluster result never claims more than the
  weakest node.
- Probe errors map to "not verified" and never advance anything. There is no path from
  a probe error to a "safe" or "Verified" result.
- Validation failures are a `Ready=False` condition with a reason (no `Failed` phase).
- Standby replicas gate on the leader flag like every other component and never
  write status; leadership is re-checked before every status write (a reconcile can
  outlive its leadership).
- A CR being deleted gets no probes, status write or rewrite: after the one-time
  finalizer strip the reconcile stops.
- Every networked call uses `tokio::time::timeout_at`.

### No finalizer; migrating the v0.1.11 object

The controller owns nothing in the cluster apart from a transient canary Secret
(deleted best-effort after each check), so it adds no finalizer and has no cleanup.
Deleting the CR just deletes it. A CR created by v0.1.11 already carries the
`platform.rye.ninja/cleanup` finalizer, which the new reconciler removes once (and
only that finalizer) so a pending deletion cannot hang. The other six components are
untouched and keep their finalizers.

### Code changes

- **Delete:** `kms_provider.rs`, `kms_barbican.rs`, `talos_patches.rs`, the DaemonSet,
  finalizer/cleanup/ledger paths in `etcd_encryption_reconciler.rs`, the acknowledgement
  machinery and the cleanup half of `encryption_phase.rs`.
- **Keep:** `secret_rewrite.rs`; the canary Secret; leader gating.
- **Rework:** `etcd_encryption.rs` (the new spec/status types, i64 counters),
  `encryption_probe.rs` (metrics parser and delta, the per-node client behind a trait),
  `encryption_phase.rs` (the pure derivation above), the reconciler (no finalizer, the
  one-time finalizer strip), `main.rs` (run the controller without the finalizer
  wrapper), `deploy/crd.yaml`, the example, README section, runbook, spec, memory.
- **RBAC ledger** (`docs/memory/rbac-cluster-admin-tradeoff.md`): drop `daemonsets`;
  keep `get/list/update` on every Secret, add `list` on `nodes`, `list` on
  `endpointslices.discovery.k8s.io`, `get` on
  `nonResourceURLs: /metrics` and `/readyz`, and canary `create/patch/get/delete` on one
  Secret in `kube-system`.

### Testing

- Pure units: the metrics parser (including the `key2:` and empty-prefix cases and
  counters that are absent when zero), the delta (including a negative delta), the
  verdict per node, the cluster-level derivation (every branch above, including mixed
  writers and "deltas below the Secret count"), spec validation (including the
  empty-`kmsProviderName` leftover-CR case) and node-target derivation.
- Orchestration against an in-memory fake of an `ApiserverProbe` trait (`readyz`,
  `metrics`, `canary_write`, `list_all_secrets`, per node, and
  `apiserver_endpoint_addresses`), covering every
  derivation outcome and that an erroring node never yields `ReadyToRemoveLegacy`.
- The existing rewrite-loop tests stay.
- The real per-node client, the TLS server-name override and the live metrics are
  covered by the runbook against a real cluster.
- A schema example test over the new `examples/etcd-encryption.yaml`.

## Open verification items

Settled only by the runbook on a real cluster:

1. Whether the apiserver's serving certificate accepts `kubernetes.default.svc` as the
   TLS server name when dialled at a node IP.
2. The exact name and presence of the `kms-providers` readiness check on the
   Kubernetes version Talos ships (observed as `[+]kms-providers ok` on 2026-10-01).
3. That a GET with no `resourceVersion` reads from etcd rather than the watch cache on
   that Kubernetes version (the completeness check on the delta guards this: cache-served
   reads would sum to fewer decrypts than Secrets read).
4. What `transformer_prefix` an identity (plaintext) read reports (not observed; the
   design treats the empty prefix as legacy).
5. Whether a counter series is absent when its value is zero (the parser treats absent
   as 0).
