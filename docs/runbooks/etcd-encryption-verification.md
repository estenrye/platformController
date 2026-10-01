# Verifying etcd Secret encryption (Barbican KMS) on Talos

Manual acceptance for the `EtcdEncryption` resource. **Not live-verified
yet**: nothing below has been run against a real cluster, and section 7 lists
the facts this run must pin down. Needs a real Talos cluster on OpenStack with
`CniInstallation` already `Ready`. `<cp>` stands for a control-plane node IP;
`<cp1>`, `<cp2>`, ... for each one in turn.

How the protocol works, in one paragraph: the controller never holds a Talos
credential, so it cannot change the apiserver's encryption config. It
installs the plugin, publishes each Talos patch in `.status.talosPatches`,
and waits for you to apply it and set the matching `spec.acknowledgements.*`
flag. It advances only when the acknowledgement **and** its own probes (the
apiserver's KMS metrics and a canary Secret round trip) agree. There is no
`Failed` phase; failures show as `Ready=False` with a reason and never move
the phase backwards or reset an acknowledgement.

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

- A baseline plaintext check. Create a Secret, then read it straight from
  etcd, bypassing the apiserver:

```sh
kubectl create secret generic plain-before --from-literal=k=v
talosctl -n <cp> etcd get /registry/secrets/default/plain-before
# or, if you have etcdctl and client certs: etcdctl get /registry/secrets/default/plain-before
```

Expected: the raw value is readable, starting with `k8s` and containing the
literal `k` / `v` bytes (no `k8s:enc:` prefix). If it is already encrypted,
stop: the cluster already has an encryption config this runbook would fight.

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

Apply to **one control-plane node at a time**, waiting for the apiserver to
come back (and the node to be Ready) before the next:

```sh
talosctl -n <cp1> patch machineconfig --patch @patch1.yaml
kubectl get --raw /readyz                  # repeat until ok, then next node
talosctl -n <cp2> patch machineconfig --patch @patch1.yaml
# ... and so on for every control-plane node
```

Then acknowledge:

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
talosctl -n <cp> etcd get /registry/secrets/default/plain-before | head -c 120
```

Expected: the value now starts with `k8s:enc:kms:v2:barbican:` and is **not**
readable.

## 4. Patch 2: remove the identity fallback

```sh
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.removeIdentity}' > patch2.yaml
# apply one control-plane node at a time, as in section 2
talosctl -n <cp1> patch machineconfig --patch @patch2.yaml
kubectl get --raw /readyz
# ... remaining nodes
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"plaintextRemoved":true}}}'
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}{.status.conditions}{"\n"}'
```

Expected: phase `Encrypted`, `Ready=True`, no `Degraded` condition.
`Encrypted` also requires the apiserver KMS probe and the canary Secret
(`kube-system/etcd-encryption-canary`) round trip to pass.

## 5. Prove plaintext is rejected

This proves the identity fallback is gone. **Do not run this on a cluster you
care about**: while the plugin is down, the apiserver cannot decrypt any
Secret.

```sh
kubectl -n kube-system patch ds barbican-kms \
  -p '{"spec":{"template":{"spec":{"nodeSelector":{"x":"y"}}}}}'
kubectl get secret plain-before -o yaml      # must fail
```

Expected: the read fails (the apiserver cannot decrypt). Restore the plugin.
The controller re-applies the DaemonSet on its next reconcile, but you can
revert it immediately:

```sh
kubectl -n kube-system patch ds barbican-kms --type json \
  -p '[{"op":"remove","path":"/spec/template/spec/nodeSelector/x"}]'
kubectl get secret plain-before -o yaml      # readable again
```

## 6. Delete protocol

Deleting after patch 1 was acknowledged is a multi-step walk; the CR stays
`Terminating` throughout and the plugin is removed **last**.

```sh
kubectl delete etcdenc default --wait=false
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}'     # RevertingKms
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.revert}' > revert.yaml
# apply to each control-plane node one at a time, wait for the apiserver
talosctl -n <cp1> patch machineconfig --patch @revert.yaml
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"kmsReverted":true}}}'
kubectl get etcdenc default -o jsonpath='{.status.phase}{"\n"}{.status.rewrite}{"\n"}'
```

The `revert` patch lists `identity` first with `kms` still present, so new
writes go plaintext while old ciphertext stays readable. After
`kmsReverted`, phase goes `Decrypting` (every Secret is re-saved) then
`AwaitingKmsRemoval`, and `status.talosPatches.removeKms` appears:

```sh
kubectl get etcdenc default -o jsonpath='{.status.talosPatches.removeKms}' > removekms.yaml
talosctl -n <cp1> patch machineconfig --patch @removekms.yaml
# ... every control-plane node
kubectl patch etcdenc default --type merge \
  -p '{"spec":{"acknowledgements":{"kmsRemoved":true}}}'
kubectl -n kube-system get ds barbican-kms          # eventually NotFound
kubectl get etcdenc default                         # eventually NotFound
talosctl -n <cp> etcd get /registry/secrets/default/plain-before | head -c 120
```

Expected: the DaemonSet disappears last, only after `kmsRemoved` is set **and**
the apiserver stops reporting an active KMS provider; the baseline Secret is
plaintext again in etcd.

Fail-safe behavior to confirm along the way:

- **Unreadable probe:** if the apiserver metrics probe cannot be read during
  deletion (for example, the apiserver is restarting), the controller treats
  KMS state as unknown and keeps the CR `Terminating`; it never removes the
  plugin on an unreadable probe. It retries on its own.
- **Delete before patch 1 is acknowledged** (`AwaitingKmsConfig`, no
  acknowledgement set): if the apiserver does not report KMS active, the
  plugin is removed immediately and the CR is gone. But if patch 1 was
  already applied and the apiserver already reports KMS active, deleting does
  **not** remove the plugin immediately even though `kmsConfigApplied` is not
  set; it walks the revert protocol above. Test both: delete a fresh
  resource that never reached patch 1, and delete one whose patch 1 was
  applied but not yet acknowledged.

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
