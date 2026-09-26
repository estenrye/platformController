# Verifying the Spegel pull-through cache on Talos

Manual acceptance for the `PullThroughCache` resource. Needs a Talos cluster with
a working CNI (`CniInstallation` at `Ready`). Step 2 (peer-to-peer serving) needs
**at least two nodes**: on a single node Spegel never becomes Ready, because its
`registry` container logs `routing table is empty after bootstrapping`, the
startup probe returns 500 and the container restarts. Steps 1 and 3 were run on
one node (see "Findings to record"). The Talos-in-Docker recipe in
`tests/integration_talos.rs` works for steps 0-3; use it with `--workers 2`.

`spec.spegel.registries` entries are registry URLs (`https://docker.io`); Spegel
rejects bare hostnames.

## 0. Node prerequisite (once per node, before applying the CR)

Spegel serves layers containerd already unpacked, but Talos's containerd discards
unpacked layers by default. Patch every node's machine config:

```yaml
# spegel-talos-patch.yaml
machine:
  files:
    - path: /etc/cri/conf.d/20-customization.part
      op: create
      permissions: 0o644
      content: |
        [plugins."io.containerd.cri.v1.images"]
          discard_unpacked_layers = false
```

```sh
talosctl patch machineconfig --nodes <node-ip> --patch @spegel-talos-patch.yaml
```

Talos reports whether the change applied live or needs a reboot; if it needs one,
allow it. This is the one thing the controller cannot check: it reports only that
its own manifests were applied.

## 1. Apply

```sh
kubectl apply -f examples/pull-through-cache.yaml
kubectl get ptc default -o jsonpath='{.status.phase}{"\n"}'    # Ready
kubectl -n spegel get ds,pods -o wide                          # one pod per node, Running
```

`Ready` means the manifests were applied, not that Spegel is healthy on every
node; the DaemonSet's pods are the real signal.

On a single node the pod is not Ready: the `configuration` init container
succeeds (with URL registries) and the `registry` container restarts, logging
`routing table is empty after bootstrapping`. That is expected without peers; with
two or more nodes, check that the pods become Ready (not yet verified).

## 2. Peer-to-peer serving

Pick an image that is not on any node yet, and two nodes (A and B):

```sh
kubectl run pull-a --image=<image> --overrides='{"spec":{"nodeName":"<node-A>"}}' --restart=Never
kubectl wait --for=condition=Ready pod/pull-a --timeout=120s
kubectl run pull-b --image=<image> --overrides='{"spec":{"nodeName":"<node-B>"}}' --restart=Never
kubectl wait --for=condition=Ready pod/pull-b --timeout=120s
```

Then confirm node B fetched from node A rather than upstream. Look at node B's
Spegel pod (`kubectl -n spegel logs <pod-on-B>`, and its `/metrics` on port 9090:
a mirror-requests counter labelled by source). The exact log line and metric name
come from the Spegel version in use (chart `0.7.4`); record what you observe
here so the next run does not have to guess.

## 3. Delete, and what nodes keep

Spegel removes the mirror config it wrote on each node through a **post-delete
Helm hook** (chart template `templates/post-delete-hook.yaml`: a `spegel-cleanup`
DaemonSet, a `spegel-cleanup-wait` Pod and a `spegel-cleanup` Service). The
controller renders with `--no-hooks`, so that hook does not run on delete.

```sh
kubectl delete ptc default          # returns once the finalizer clears
kubectl get ns spegel               # NotFound once terminating finishes
talosctl -n <node-ip> ls /etc/cri/conf.d/hosts     # do mirror configs remain?
```

Then, on a node, pull an image that no node has and that is under a mirrored
registry:

```sh
kubectl run after-delete --image=<uncached image> --restart=Never
kubectl wait --for=condition=Ready pod/after-delete --timeout=120s
```

- **Pull succeeds (observed on a single node, 2026-09-25):** containerd falls
  back to the upstream registry. There the pull of `registry.k8s.io/pause:3.8`
  took ~1.5s with the post-delete hook not run. Caveats: the mirror never served
  content, and leftover mirror config on the node was not inspected. Repeat it on
  a multi-node cluster where the mirror was serving before you rely on it.
- **Pull hangs or fails:** only if a multi-node run shows it (not observed so
  far). Then cleanup needs a node-cleanup step: stop and revisit the spec before
  merging. As a manual workaround, run only the hook objects. Step 3 already deleted the
  `spegel` namespace, so recreate it first with the privileged pod-security
  labels (the hostPath cleanup DaemonSet is rejected without them), and pass the
  Talos config path (the chart default is `/etc/containerd/certs.d`, which would
  clean the wrong directory):

  ```sh
  kubectl create ns spegel
  kubectl label ns spegel pod-security.kubernetes.io/enforce=privileged \
    pod-security.kubernetes.io/audit=privileged pod-security.kubernetes.io/warn=privileged
  helm template spegel oci://ghcr.io/spegel-org/helm-charts/spegel --version 0.7.4 \
    --namespace spegel --set spegel.containerdRegistryConfigPath=/etc/cri/conf.d/hosts \
    --show-only templates/post-delete-hook.yaml | kubectl apply -f -
  kubectl -n spegel wait --for=jsonpath='{.status.phase}'=Succeeded pod/spegel-cleanup-wait --timeout=180s
  kubectl delete ns spegel
  ```

  `spegel-cleanup-wait` is the chart's own completion signal: it exits once every
  `spegel-cleanup` DaemonSet pod has cleaned its node.

## Findings to record

Steps 1 and 3 have been run on one IPv6-only node and recorded in
`docs/memory/pull-through-cache-2026-09.md`. Step 2 is still to do on at least two
nodes: write the peer-serving signal there, and whether fail-open holds when the
mirror was serving.
