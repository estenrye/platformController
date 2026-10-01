# Adopting and verifying KMS Secret encryption on Talos

Manual acceptance for the `EtcdEncryption` resource (adopt mode, which replaced
the v0.1.11 fresh-install flow). Needs a real Talos cluster whose apiserver
already encrypts Secrets through a KMS provider. **EXPERIMENTAL and NOT
live-verified end to end: do not run it on a cluster you care about.** Every
command marked "unverified" has not been run against a live cluster. See
"Findings to record" at the end and `docs/memory/etcd-encryption-2026-09.md`.

## 0. What this does and does not do

It **observes** each control-plane apiserver, optionally **rewrites** every
Secret, and **proves per apiserver** that nothing is stored under a legacy
provider (for example Talos's default secretbox), before telling you it is safe
to remove that provider.

It does **not** install the KMS plugin, enable KMS, publish Talos patches or edit
your `EncryptionConfiguration`. You do all of that; the controller only reports.

Design basis, from running the old controller against a real cluster on
2026-10-01:

- The KMS plugin already ran as Talos static pods (`barbican-kms-plugin-<node>`)
  sharing `/var/lib/kms/kms.sock`.
- KMS was already configured with Talos's secretbox kept as a read fallback.
  The apiserver's own metrics showed it:

  ```
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="to_storage",  transformer_prefix="k8s:enc:kms:v2:barbican:"} 1
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 284
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:secretbox:v1:"} 86
  apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="key2:"} 86
  ```

  New writes already used KMS; 86 Secrets were still stored under secretbox. The
  inner `key2:` prefix is a sub-prefix of the same secretbox operation; the
  controller counts only top-level (`k8s:enc:`) prefixes.

## 1. Prerequisites

1. The apiserver already uses a KMS provider:

   ```sh
   kubectl get --raw '/readyz?verbose' | grep kms-providers
   ```

   Expected: `[+]kms-providers ok`.

2. A baseline of provider use (the `to_storage` prefix is your target provider):

   ```sh
   kubectl get --raw /metrics | grep apiserver_storage_transformation_operations_total | grep secrets
   ```

   Expected: a `to_storage` line whose `transformer_prefix` is
   `k8s:enc:kms:v2:<name>:`, where `<name>` is the `name` of the kms entry in
   your EncryptionConfiguration (this becomes `spec.kmsProviderName`).

3. Take an etcd snapshot before changing anything:

   ```sh
   talosctl -n <cp> etcd snapshot db.snapshot
   ```

   Expected: `db.snapshot` written locally.

4. Network: the controller pod must reach every control-plane node's apiserver
   at `https://<InternalIP>:6443`, and that apiserver's serving certificate must
   accept the server name `kubernetes.default.svc` (the controller dials the node
   IP but sets that TLS server name). This is **unverified**; see section 6.

## 2. Migrating from v0.1.11

The schema changed (`provider`, `barbican` and the old acknowledgements were
removed; `kmsProviderName` is new and required). Delete the old object first:

```sh
kubectl delete etcdenc default
kubectl get etcdenc default
```

Expected: the new controller removes the v0.1.11 finalizer
(`platform.rye.ninja/cleanup`) and the object disappears (`NotFound`). The
controller no longer has any cleanup to do. If you scaled the controller to 0
earlier, scale it back to 2:

```sh
kubectl -n platform-system scale deployment/platform-controller --replicas=2
```

(Adjust the namespace/name if you changed them in `deploy/bootstrap.yaml`.)
Expected: two ready replicas; one is the leader. A pending deletion cannot
finish while the controller is scaled to 0, because only the controller strips
the old finalizer.

## 3. Apply and observe

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/etcdencryptions.platform.rye.ninja
# run the controller at the new image, then:
kubectl apply -f examples/etcd-encryption.yaml   # set spec.kmsProviderName first
kubectl get etcdenc default -o yaml
```

Expected: within a minute `status.phase` and `status.nodes[]` are populated, one
entry per control-plane node.

The phase is derived from scratch on every reconcile; nothing is remembered.

| Phase | Meaning |
|---|---|
| `Observing` | Cannot yet vouch for the cluster: a node is unverifiable, writers are mixed, the apiserver is not writing with the target, legacy objects remain and `rewrite` is `Disabled`, or reads could not be confirmed. Read the `Ready` condition message. Requeued every 30 s. |
| `NotConfigured` | No node reports a `kms-providers` readiness check and none writes with the target. The controller does not enable KMS. Requeued every 600 s. |
| `Migrating` | Every apiserver writes with the target, at least one node completely read legacy objects, and `rewrite: Enabled`: the controller is rewriting every Secret. Requeued every 30 s. |
| `ReadyToRemoveLegacy` | Every apiserver writes with the target and reads every Secret with zero legacy reads. It is safe (subject to the confirmations in section 5) to remove the legacy providers. Requeued every 600 s. |
| `Verified` | As above, **and** you set `acknowledgements.legacyProvidersRemoved: true`, **and** the canary Secret round-tripped. Means "the probes agree and you acknowledged removal", nothing more. `Ready=True`. Requeued every 600 s. |

There is no `Failed` phase. A validation failure (empty `kmsProviderName`, or
one containing whitespace or `:`) is `Ready=False` with reason `InvalidSpec`;
an unsupported `platformKind` or a name other than `default` is reason
`Unsupported`.

Per node (`status.nodes[]`):

- `name`, `address`: the Node and its `InternalIP`.
- `verified`: true only if the node writes with the target **and** its reader
  check was complete with zero legacy reads.
- `writerPrefix`: the top-level prefix the node wrote the canary with; `null`
  if it could not be determined.
- `readsByPrefix`: what the node's apiserver decrypted while listing every
  Secret, by top-level prefix.
- `secretsListed`: how many Secrets that list returned.
- `reason`: empty when clean; otherwise why not.

`status.legacyPrefixes` lists every top-level prefix other than the target that
was read with a count above zero (for example `k8s:enc:secretbox:v1:`). A read
reported with an empty prefix (what identity/plaintext presumably reports; not
yet observed) is also legacy.

`status.conditions[?(@.type=="Ready")]` carries the phase as its reason and
the derivation's explanation as its message:

```sh
kubectl get etcdenc default -o jsonpath='{range .status.conditions[*]}{.type}={.status} {.reason}: {.message}{"\n"}{end}'
kubectl get etcdenc default -o jsonpath='{range .status.nodes[*]}{.name}{"\t"}{.verified}{"\t"}{.writerPrefix}{"\t"}{.reason}{"\n"}{end}'
```

Common reasons:

- `cannot verify node X: ...`: the node could not be probed (see section 6).
  The phase stays `Observing`; the status never claims safe.
- `NotConfigured` (`no KMS provider is active; this controller does not enable
  KMS`): nothing configured; do that yourself.
- `a KMS provider is configured but the apiserver is not writing with <prefix>`:
  `spec.kmsProviderName` does not match the entry name, or the KMS provider is
  not listed first.
- `mixed: some apiservers do not write with the target provider yet`: a rolling
  apiserver restart; wait. It never starts a rewrite.
- `legacy objects remain; set rewrite: Enabled to migrate`.
- `cannot verify reads on every apiserver; not rewriting` / `cannot verify
  reads: ...`: a node's list was not provably complete (fewer objects were
  decrypted than listed) or errored. The controller deliberately does not
  rewrite on an unconfirmable read, because it would repeat full rewrites
  every 30 s without ever being able to confirm.

### Staleness: is the controller actually verifying?

`status.phase` is not a live feed; it is what the last successful reconcile
wrote. The `Ready` condition's `lastTransitionTime` is refreshed on **every**
reconcile (it is not only set on a change), so it is the heartbeat:

```sh
kubectl get etcdenc default -o jsonpath='{.status.conditions[?(@.type=="Ready")].lastTransitionTime}{"\n"}'
date -u +%FT%TZ
```

Expected: a timestamp within roughly the last 30 seconds (phases `Observing`,
`Migrating`) or 10 minutes (`NotConfigured`, `ReadyToRemoveLegacy`,
`Verified`), plus the duration of a full reconcile. An older timestamp means
the controller is not verifying (not running, not the leader, or crash-looping):
treat the phase as **stale** and do not act on it. Check
`kubectl -n platform-system logs deploy/platform-controller` and the Lease
holder.

## 4. Migrate

Only once every node writes with the target and `legacyPrefixes` is not empty:

```sh
kubectl patch etcdenc default --type merge -p '{"spec":{"rewrite":"Enabled"}}'
kubectl get etcdenc default -o jsonpath='{.status.phase} {.status.rewrite}{"\n"}' -w
```

Expected: `Migrating`; `status.rewrite.total` / `rewritten` rising page by page
(a restart simply repeats the pass); on a later reconcile, `ReadyToRemoveLegacy`
once every apiserver reads every Secret with zero legacy reads.

The rewrite re-saves every Secret in the cluster **unchanged** (a `replace`
with the object just read), so each is stored again through the current write
provider. A conflict is retried, a missing Secret counts as done, and the
controller never logs Secret data; it logs the namespace and name only.

It runs only with `rewrite: Enabled`, only in the `Migrating` derivation. It
needs `get`/`list`/`update` on every Secret in every namespace.

### A Secret that will not rewrite

If a Secret permanently fails to be re-saved (typically an admission webhook
rejects the update), `status.rewrite.failed` stays above 0, the phase stays
`Migrating` (the Secret keeps its legacy encoding, so reads stay legacy), and
the whole cluster-wide rewrite repeats every 30 seconds. Find the failing
Secrets by namespace/name in the controller log:

```sh
kubectl -n platform-system logs deploy/platform-controller | grep "failed to rewrite secret"
```

Expected: lines carrying `namespace=` and `secret=` fields, never data. If
several replicas run, check the leader's pod. Then either fix the webhook (or
whatever rejects the update) so the controller's next pass succeeds, or, once
you have confirmed you can recreate it, delete and recreate that Secret
deliberately. Setting `rewrite: Disabled` stops the repeating passes.

## 5. Remove the legacy provider (your change)

The controller does not do this. Before you do:

### 5a. Confirm the reader check on a quiet cluster

The reader check lists every Secret with a `limit` and no `resourceVersion`,
so the apiserver reads from etcd (not its watch cache) and decrypts every
object, and counts `from_storage` per prefix. The check is evidence, not
proof: ambient reads by other clients can inflate the counts. Confirm the
assumption on a quiet cluster (or by reading the counters twice around a list),
on each control-plane apiserver in turn (use that node's address, or run
through each node):

```sh
SECRETS=$(kubectl get secrets -A --no-headers | wc -l)
kubectl get --raw /metrics | grep 'apiserver_storage_transformation_operations_total' | grep 'resource="secrets"' | grep from_storage > before.txt
kubectl get secrets -A --chunk-size=100 -o name > /dev/null
kubectl get --raw /metrics | grep 'apiserver_storage_transformation_operations_total' | grep 'resource="secrets"' | grep from_storage > after.txt
diff before.txt after.txt
```

Expected: the `from_storage` counters (summed over the top-level `k8s:enc:`
prefixes, ignoring the inner `key2:`) rise by **at least** `$SECRETS`. If they
rise by less, the list was served from the watch cache, the controller's
completeness check will report "cannot verify reads" for that node, and you
must not rely on it. (Through the load balancer `kubectl` reaches one apiserver
at a time; to be per apiserver, query each node's `https://<node-ip>:6443`.)

### 5b. Confirm with an etcd snapshot (unverified command)

Metrics are not the final word. Before removing the provider, take a fresh
snapshot and look at the stored values:

```sh
talosctl -n <cp> etcd snapshot db.snapshot
```

Then inspect the keys under `/registry/secrets/` for their value prefixes,
using `etcdctl` or a snapshot reader. The exact command is **unverified**
(`talosctl etcd get` does not exist); for example, restore the snapshot into a
scratch `etcd` and run something like
`etcdctl get --prefix /registry/secrets/ --print-value-only | grep -a -o -E 'k8s:enc:[a-z0-9]+(:v[0-9]+)?:' | sort | uniq -c`.
Expected: only `k8s:enc:kms:v2:` (your target) prefixes, no
`k8s:enc:secretbox:`. If any other prefix remains, do not remove that provider.

### 5c. Remove the provider, one control-plane node at a time

Remove secretbox (and any other legacy provider) from your Talos
`KubeEtcdEncryptionConfig`, **one control-plane node at a time**. After each
node, wait until that node's `kube-apiserver-<node>` pod has a start time
after the patch:

```sh
kubectl -n kube-system get pod kube-apiserver-<node> -o jsonpath='{.status.startTime}{"\n"}'
```

Expected: a time later than your `talosctl patch` of that node. Do not use
`/readyz` for this: it goes through a load balancer and does not prove the
patched node restarted.

Then acknowledge:

```sh
kubectl patch etcdenc default --type merge -p '{"spec":{"acknowledgements":{"legacyProvidersRemoved":true}}}'
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[?(@.type=="Ready")].status}{"\n"}'
```

Expected: `Verified` and `True`. The metrics **cannot prove** the apiserver
configuration no longer lists the legacy provider; `Verified` means every
apiserver wrote and read only with the target during the check, the canary
round-tripped, and you said you removed the legacy providers. If you removed it
too early the reads would fail (Secrets unreadable); that is why 5a and 5b come
first and why you keep the snapshot from section 1.

## 6. Troubleshooting

- **`tls: bad certificate` / `certificate is valid for ... not ...`** in a
  node's `reason`: the apiserver's serving certificate does not accept the
  server name `kubernetes.default.svc` at a node IP (unverified assumption).
  Record the exact error and the certificate's SANs as a finding:

  ```sh
  openssl s_client -connect <node-ip>:6443 </dev/null 2>/dev/null | openssl x509 -noout -ext subjectAltName
  ```

  Expected: a SAN list. The status says "cannot verify" for that node and never
  claims safe; the controller has no workaround until this is fixed in code.
- **A node `Unverifiable` / timeout**: reachability. The controller pod must
  reach `https://<InternalIP>:6443` of every Node labelled
  `node-role.kubernetes.io/control-plane` (NetworkPolicy, firewall). Check
  `kubectl get nodes -l node-role.kubernetes.io/control-plane -o wide`. A
  discovery failure shows as `Ready=False` reason `VerificationFailed`.
- **Counters "went backwards"**: an apiserver restarted mid-check; the node is
  unverifiable for that run and it retries on the next reconcile.
- **`the canary write was not counted by any provider`**: the canary write did
  not raise any `to_storage` counter; check the metrics name/labels (finding e).
- **Phase looks old**: see "Staleness" in section 3.

## 7. Findings to record

The spec's open items, settled only by running this:

- (a) Did TLS to `https://<node-ip>:6443` with server name
  `kubernetes.default.svc` work?
- (b) Is the `kms-providers` readiness line named exactly so on this Talos /
  Kubernetes version (observed as `[+]kms-providers ok` on 2026-10-01)?
- (c) Did the limit-paged list read from etcd (section 5a; the reader check's
  completeness guards this: note any node reported incomplete)?
- (d) What `transformer_prefix` do plaintext/identity reads report (create an
  unencrypted object only on a throwaway cluster)?
- (e) Is a zero-value counter series absent (the parser treats absent as 0)?

Update `src/transformation_metrics.rs` / `src/apiserver_probe.rs` and their
tests if any differ.

## 8. Known limitations

- No plugin installation, no Talos patches, no key management: you own the KMS
  plugin, the key and the EncryptionConfiguration.
- The reader check lists **all** Secrets on **every** control-plane node on each
  run (steady state every 10 minutes, active phases every 30 s), which is not
  cheap on a large cluster.
- The apiserver port is fixed at 6443.
- The reader check is evidence, not proof; confirm with an etcd snapshot (5b).
- Only Secrets are covered.
