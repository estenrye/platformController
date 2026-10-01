# Verifying etcd Secret encryption (Barbican KMS) on Talos

Manual acceptance for the `EtcdEncryption` resource. **EXPERIMENTAL, not
live-verified**: nothing below has been run against a real cluster, and
section 7 lists the facts this run must pin down. Do not run it on a cluster
you care about; read section 8 (Known limitations) first. Needs a real
Talos cluster on OpenStack with `CniInstallation` already `Ready`. `<cp>` stands for a control-plane node IP;
`<cp1>`, `<cp2>`, ... for each one in turn.

How the protocol works, in one paragraph: the controller never holds a Talos
credential, so it cannot change the apiserver's encryption config. It
installs the plugin, publishes each Talos patch in `.status.talosPatches`,
and waits for you to apply it and set the matching `spec.acknowledgements.*`
flag. It advances only when the acknowledgement **and** its own probes (the
apiserver's KMS metrics, a canary Secret round trip and, where it matters,
listing every Secret) agree. There is no `Failed` phase; failures show as
`Ready=False` with a reason and never move the phase backwards or reset an
acknowledgement.

**An acknowledgement only counts if you set it after its patch appeared.**
The controller records, in `.status.patchGenerations`, the object's
`metadata.generation` at the write that first published each patch; an
acknowledgement counts only once the generation is higher (that is, the spec
was changed after the patch was published). If an acknowledgement was already
`true` when its patch appeared, it is ignored: apply the patch, then flip the
acknowledgement to `false` and back to `true` (two `kubectl patch` calls).
While a later acknowledgement is also set, the `false` step may briefly report
`InvalidAcknowledgements`; that is harmless and clears on the second call.

**Applying a patch "to every control-plane node"** always means, in this
runbook: one node at a time, and after each node wait until **that node's**
apiserver has restarted with the new config before touching the next:

```sh
date -u +%Y-%m-%dT%H:%M:%SZ                 # note the time, T
talosctl -n <cpN> patch machineconfig --patch @patchX.yaml
kubectl -n kube-system get pod kube-apiserver-<cpN-node-name> \
  -o jsonpath='{.status.startTime}{"\n"}'    # repeat until later than T
kubectl -n kube-system get pod kube-apiserver-<cpN-node-name>   # Running, 1/1
```

`kubectl get --raw /readyz` is **not** enough: it goes through the load
balancer to any apiserver, so it does not prove the patched node restarted.
Set the acknowledgement only after **every** control-plane node is done.

## 0. Prerequisites

- A Talos cluster, `CniInstallation` `Ready`, the controller running with all
  seven CRDs Established.
- A 256-bit AES key in Barbican, and a `cloud.conf` containing both the
  OpenStack credentials and `[KeyManager] key-id`. **Losing this key makes
  every Secret unreadable.** Create the Secret (the key must be `cloud.conf`):

```sh
kubectl -n kube-system create secret generic barbican-kms-cloud-config \
  --from-file=cloud.conf=./cloud.conf
```

- **Check for an existing encryption provider.** Talos's default
  `talosctl gen config` already encrypts Secrets with secretbox
  (`cluster.secretboxEncryptionSecret`). The controller cannot see the
  machine config, and every patch it generates **replaces** the `providers`
  list. On each control-plane node:

```sh
talosctl -n <cp> get machineconfig -o yaml | grep -n -E 'secretboxEncryptionSecret|aescbcEncryptionSecret|KubeEtcdEncryptionConfig'
```

  If anything matches, the cluster already encrypts Secrets. Before applying
  **each** patch from this runbook, splice that provider into the patch's
  `providers` list as a read fallback, **after** the generated entries (and
  before `identity` if the patch lists it last), exactly as the warning
  comment at the top of every generated patch says. For secretbox that is an
  entry like `- secretbox: {keys: [{name: key1, secret: <the existing
  secret>}]}` (unverified shape: copy the existing provider block verbatim
  from the machine config). Leaving it out makes every existing Secret
  unreadable. Keep it in every later patch, including the deletion patches.

- A baseline etcd read. `talosctl` has no `etcd get`; read etcd with
  `etcdctl` using an etcd client certificate and key extracted per the Talos
  etcd documentation (**the exact commands are unverified**; record what
  worked in section 7):

```sh
kubectl create secret generic plain-before --from-literal=k=plaintext-marker-7f3a
etcdctl --endpoints https://<cp>:2379 --cacert etcd-ca.crt \
  --cert etcd-client.crt --key etcd-client.key \
  get /registry/secrets/default/plain-before | head -c 200
# alternative (unverified): take a snapshot and search it
talosctl -n <cp> etcd snapshot db.snapshot
grep -a -c plaintext-marker-7f3a db.snapshot
```

  Expected without a pre-existing provider: the raw value starts with `k8s`
  and contains `plaintext-marker-7f3a` (no `k8s:enc:` prefix). If it starts
  with `k8s:enc:secretbox:` (or another `k8s:enc:` prefix), the cluster
  already encrypts Secrets: go back to the previous bullet and splice that
  provider into every patch. A snapshot holds old revisions until etcd is
  compacted, so the snapshot method can still show plaintext after
  encryption; prefer `etcdctl get`.

## 1. Apply and reach `AwaitingKmsConfig`

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/etcdencryptions.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/etcd-encryption.yaml
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}'
kubectl -n kube-system get ds barbican-kms -o wide
kubectl -n kube-system get pods -l k8s-app=barbican-kms -o wide
talosctl -n <cp> ls /var/lib/kms/          # repeat per control-plane node; expect kms.sock
```

Expected: phase passes through `InstallingPlugin` and settles at
`AwaitingKmsConfig`. The DaemonSet has one Ready pod on **every** control-plane
node (the controller requires the DaemonSet rollout to be current, with
observed generation and updated pods, and a Ready pod on every control-plane
node, not just one; it will sit in `InstallingPlugin` otherwise). `kms.sock`
exists on each node. `.status.talosPatches.enableKms` is populated.

## 2. Patch 1: enable KMS

```sh
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.enableKms}' > patch1.yaml
```

If section 0 found an existing provider, splice it into `patch1.yaml` now.
This is the only patch carrying the `extraVolumes` document that mounts
`/var/lib/kms` into the apiserver; later patches rely on it staying in the
machine config.

Apply to every control-plane node, one at a time, waiting for each node's
apiserver to restart as described at the top of this runbook:

```sh
talosctl -n <cp1> patch machineconfig --patch @patch1.yaml
# wait for kube-apiserver-<cp1 node> startTime to move past the patch time
talosctl -n <cp2> patch machineconfig --patch @patch1.yaml
# ... and so on for every control-plane node
```

Then, only after every node is done, acknowledge (if `kmsConfigApplied` was
already `true`, flip it `false` then `true`):

```sh
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"kmsConfigApplied":true}}}'
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}'
kubectl get etcdenc default -o jsonpath='{.status.rewrite}{"\n"}'
```

Expected: phase moves to `Rewriting` then `AwaitingPlaintextRemoval`;
`status.rewrite.failed` is `0` and `rewritten` equals `total`. Rewriting runs
**only while the apiserver reports an active KMS provider**: if it doesn't,
the phase stays `Rewriting` and the Ready condition message says it is
waiting for the apiserver to report an active KMS provider before rewriting
Secrets. In that case check the patch landed on every node and the plugin
pods are healthy; do not force anything.

## 3. Verify encryption in etcd

```sh
etcdctl --endpoints https://<cp>:2379 --cacert etcd-ca.crt \
  --cert etcd-client.crt --key etcd-client.key \
  get /registry/secrets/default/plain-before | head -c 120   # unverified, see section 0
```

Expected: the value now starts with `k8s:enc:kms:v2:barbican:` and
`plaintext-marker-7f3a` does **not** appear.

## 4. Patch 2: remove the identity fallback

```sh
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.removeIdentity}' > patch2.yaml
# splice in any pre-existing provider (section 0), then apply node by node,
# waiting for each node's apiserver to restart (top of this runbook)
talosctl -n <cp1> patch machineconfig --patch @patch2.yaml
# ... remaining nodes; acknowledge only after every node is done
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"plaintextRemoved":true}}}'
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}{.status.conditions}{"\n"}'
```

Expected: phase `Encrypted`, `Ready=True`, no `Degraded` condition.
`Encrypted` also requires the apiserver KMS probe, the canary Secret
(`kube-system/etcd-encryption-canary`) round trip, and listing every Secret
(each must decrypt) to pass. All of these go through one apiserver behind the
load balancer, which is why the per-node wait above matters.

## 5. Optional destructive experiment

**Optional. Do not run this on a cluster you care about.** It does **not**
prove the identity fallback is gone: KMS v2 caches data encryption keys in
the apiserver, so with the plugin stopped a read of an existing Secret may
still succeed, and a failed read only shows the plugin is needed, not that
`identity` was removed. Section 3 (reading etcd) is the real evidence.

```sh
kubectl -n kube-system patch ds barbican-kms \
  -p '{"spec":{"template":{"spec":{"nodeSelector":{"x":"y"}}}}}'
kubectl create secret generic new-while-down --from-literal=k=v   # expected to fail
# a restarted apiserver has no cached key: optionally restart one and read
kubectl get secret plain-before -o yaml
```

Then restore the plugin **yourself**. The controller does not undo this: its
server-side apply never removes a `nodeSelector` key owned by another field
manager (`kubectl patch`), so the DaemonSet stays unschedulable until you
remove it:

```sh
kubectl -n kube-system patch ds barbican-kms --type json \
  -p '[{"op":"remove","path":"/spec/template/spec/nodeSelector/x"}]'
kubectl -n kube-system get pods -l k8s-app=barbican-kms -o wide   # Ready on every node
kubectl get secret plain-before -o yaml      # readable
kubectl delete secret new-while-down --ignore-not-found
```

## 6. Delete protocol

Deleting after patch 1 was acknowledged is a multi-step walk; the CR stays
`Terminating` throughout and the plugin is removed **last**.

```sh
kubectl delete etcdenc default --wait=false
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}'     # RevertingKms
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.revert}' > revert.yaml
# splice in any pre-existing provider (section 0); apply node by node,
# waiting for each node's apiserver to restart; acknowledge after the last
talosctl -n <cp1> patch machineconfig --patch @revert.yaml
# ... every control-plane node
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"kmsReverted":true}}}'
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}{.status.rewrite}{"\n"}'
```

The `revert` patch lists `identity` first with `kms` still present, so new
writes go plaintext while old ciphertext stays readable. If you set
`kmsReverted` before the revert patch appeared (for example before deleting),
it does not count: flip it `false` then `true` after applying the patch.
Throughout deletion the controller keeps re-applying the plugin DaemonSet,
and it rewrites Secrets only while the plugin is Ready on every control-plane
node. After `kmsReverted`, phase goes `Decrypting` (every Secret is re-saved)
then `AwaitingKmsRemoval`, and `status.talosPatches.removeKms` appears:

```sh
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.removeKms}' > removekms.yaml
# splice in any pre-existing provider (section 0) first
talosctl -n <cp1> patch machineconfig --patch @removekms.yaml
# ... every control-plane node, waiting for each node's apiserver to restart
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"kmsRemoved":true}}}'
kubectl -n kube-system get ds barbican-kms          # eventually NotFound
kubectl get etcdenc default                         # eventually NotFound
etcdctl ... get /registry/secrets/default/plain-before | head -c 120   # as in section 3
```

Expected: the DaemonSet disappears last, only after `kmsRemoved` counts (set
after `removeKms` appeared), the apiserver stops reporting an active KMS
provider, **and** every Secret can still be listed; the baseline Secret is
plaintext again in etcd (or under the pre-existing provider's prefix, if you
spliced one in).

Fail-safe behavior to confirm along the way:

- **Unreadable probe:** if the apiserver metrics probe cannot be read during
  deletion (for example, the apiserver is restarting), the controller treats
  KMS state as unknown and keeps the CR `Terminating`; it never removes the
  plugin on an unreadable probe. It retries on its own.
- **Delete early** (`Pending` or `InstallingPlugin`): the plugin is removed
  immediately only if the apiserver positively reports no KMS provider. If
  the probe is unreadable or reports KMS active, deletion walks the revert
  protocol above.
- **Delete in `AwaitingKmsConfig`:** if you applied patch 1 to **any**
  control-plane node, set `kmsConfigApplied: true` **before** deleting. That
  raw value (counted or not) keeps the plugin and walks the revert protocol.
  Setting it may let the controller advance to `Rewriting` before your delete
  lands, so first finish applying patch 1 to **every** control-plane node
  (the cluster is then consistent whichever happens first), then set the
  acknowledgement, then delete.
  Without it, the controller relies only on the KMS probe: if it reports no
  KMS provider (for example the one apiserver it reached was not yet
  patched), the plugin is removed while a patched apiserver may depend on it.
  Test both: delete a fresh resource that never reached patch 1, and delete
  one whose patch 1 was applied and `kmsConfigApplied` set.

## 7. Findings to record

Open verification items from the spec. For each, record the observed value
here and fix the code if it differed:

- (a) The exact `apiserver_envelope_encryption_*` metric names seen at
  `kubectl get --raw /metrics | grep envelope`. Update `KMS_METRIC_PREFIXES`
  in `src/encryption_probe.rs` if the prefix is wrong; the phase probes
  depend on it (a wrong prefix leaves the CR stuck at `AwaitingKmsConfig`).
- (b) Whether Talos needed `cluster.apiServer.extraVolumes` as generated or a
  different mounting mechanism, the exact accepted shape of the
  `KubeEtcdEncryptionConfig` KMS block, and whether `talosctl patch
  machineconfig` accepts the generated two-document patch (the volume patch
  and the `KubeEtcdEncryptionConfig` document separated by `---`). Correct
  `src/talos_patches.rs` and its tests if any differed.
- (c) The image tag that worked (`examples/etcd-encryption.yaml` pins
  `registry.k8s.io/provider-os/barbican-kms-plugin:v1.36.0` unverified).
- (d) Whether any admission webhook rejected a no-op Secret `update` during
  the rewrite (such Secrets are reported as `status.rewrite.failed`).
- (e) The etcd read method that worked (section 0): how the etcd client
  certificate and key were obtained on this Talos version, and whether
  `talosctl etcd snapshot` plus a search is usable.
- (f) Whether secretbox (or another provider) was already configured, and the
  exact block spliced into the patches.

## 8. Known limitations

- **Existing encryption providers are not detected or merged.** Talos's
  default config enables secretbox; you must splice it into every patch
  (section 0). Forgetting makes every existing Secret unreadable.
- **Key rotation is unsupported.** Changing the Barbican key, `cloud.conf`'s
  `key-id`, or `spec.barbican.cloudConfigSecretRef` after `Rewriting` started
  makes Secrets encrypted under the old key unreadable.
- **New control-plane nodes.** A control-plane node added after `Encrypted`
  must receive the same patches (patch 1, then patch 2) and have a Ready
  plugin pod before its apiserver serves traffic.
- **One apiserver at a time.** Every probe (KMS metrics, canary, listing
  Secrets) reaches whichever apiserver the load balancer picks; the per-node
  wait above is what covers the others.
- **Not live-verified.** See section 7.
