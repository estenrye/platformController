# Planned automation testing

Every live-cluster scenario that was run by hand while building and hardening the
controller, written down so each can be turned into an automated test. It records
what to do, what to assert, what was observed, and the mistakes that cost time.

This is a specification for future automation, not a description of tests that
exist. The tests that do exist are listed first so nobody automates them twice.

## Contents

1. [What is already automated](#1-what-is-already-automated)
2. [The environment these scenarios ran in](#2-the-environment-these-scenarios-ran-in)
3. [How to read a scenario](#3-how-to-read-a-scenario)
4. [Scenario catalogue](#4-scenario-catalogue)
5. [Lessons that will break your test code](#5-lessons-that-will-break-your-test-code)
6. [Helper snippets](#6-helper-snippets)
7. [Suggested automation design](#7-suggested-automation-design)
8. [Not yet tested](#8-not-yet-tested)

## 1. What is already automated

| Layer | Where | Runs in CI | What it covers |
|---|---|---|---|
| Unit tests | `src/**` (`cargo test --lib`) | yes | Validation, values building, ledger arithmetic, error-to-reason mapping, the IPv6 mirror-target patch, CRD schema shape |
| Manifest tests | `tests/bootstrap_manifests.rs`, `tests/ipv6_example.rs`, `tests/pull_through_cache_example.rs` | yes | `deploy/*.yaml` and `examples/*.yaml` parse, are valid, and `deploy/crd.yaml` is not stale |
| Real-chart tests | `src/helm.rs` (`#[ignore]`) | no (needs network + `helm`) | Real Calico and Spegel charts render, contain what the controller assumes, and the OCI stdout preamble is stripped |
| Live-cluster tests | `tests/integration_talos.rs`, `tests/integration_pull_through_cache.rs`, `tests/leader_election.rs` (`#[ignore]`) | no (needs a Talos cluster) | CNI install to node Ready; `PullThroughCache` apply and cleanup; leader election. Each documents its own setup in the module header |

Run the ignored real-chart tests with `cargo test --lib spegel_ -- --ignored` (and
`v3_32_1` / `render_produces` for Calico). Everything in section 4 that is marked
**Manual** has never run as code.

## 2. The environment these scenarios ran in

Results in this document are baselines from one environment, not guarantees.
Re-measure before turning any number into a threshold.

| Item | Value |
|---|---|
| Cluster | 6 nodes: 3 control-plane, 3 workers; Talos Linux v1.14.0, Kubernetes v1.37.0, containerd 2.3.4 |
| Network | IPv6-only, one `/64` for nodes, BGP to an external gateway, Calico v3.32.1 (operator-managed), no encapsulation |
| Node addressing | DHCPv6 gives each node a fixed `::NN` address; the control-plane VIP is a `/128` on one control-plane node |
| Cloud | OpenStack-based; a security group with a same-group "all traffic" rule plus port rules by CIDR |
| Controller image | `estenrye/platform-controller` 0.1.0 to 0.1.2 (`linux/amd64`) |
| Spegel | Helm chart `0.7.4` from `oci://ghcr.io/spegel-org/helm-charts/spegel` |
| Client tools | `kubectl`, `helm`, `talosctl` (**must match the server minor version**, see section 5), `gh`, `curl` |

Nodes are referred to as `NODE_A`, `NODE_B`, `NODE_C` (workers) and `CP_n`
(control-plane). Never hard-code addresses in tests; read them from the Node
objects (`status.addresses[?(@.type=="InternalIP")]`).

## 3. How to read a scenario

| Field | Meaning |
|---|---|
| **Risk** | `safe` changes nothing lasting; `disruptive` restarts pods or nodes; `destructive` removes the component under test (must restore it) |
| **Status** | `Automated` (a test exists), `Partial` (unit-level only), or `Manual` (run by hand, no code) |
| **Assert** | What a test should check. Prefer these over the observed numbers |
| **Observed** | What the manual run measured. Use as a starting point for tolerances |

Ordering rule for a suite: run every `safe` scenario first, then `disruptive`, and
run `destructive` and the full-restart scenario last.

## 4. Scenario catalogue

### 4.1 Controller and CRDs

**CTL-1 CRDs are accepted by a real API server**
Risk `safe`. Status Partial (freshness is unit-tested; acceptance is not).
1. `kubectl apply -f deploy/crd.yaml`
2. `kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja crd/pullthroughcaches.platform.rye.ninja`

Assert: both CRDs Established; `PullThroughCache` served schema lists
`spec.spegel.helmValues` as `type: object` with `x-kubernetes-preserve-unknown-fields`;
`CniInstallation` schema lists `nodeAddressAutodetectionV6Method` with enum
`cidrs`, `kubernetesInternalIP`. Applying twice reports `unchanged`. A `uint32`
format warning is expected and harmless.

**CTL-2 In-cluster bootstrap**
Risk `disruptive`. Status Manual.
1. Apply `deploy/crd.yaml`, then `deploy/bootstrap.yaml`.
2. `kubectl -n platform-system rollout status deploy/platform-controller --timeout=240s`

Assert: 2/2 ready, 0 restarts, image equals the tag in `deploy/bootstrap.yaml`,
exactly one holder in `lease/platform-controller-leader`, the standby logs only its
"connected" line. Observed: ready in about 30 s; pulls from Docker Hub worked on an
IPv6-only node.

**CTL-3 Rolling update to a new controller image**
Risk `disruptive`. Status Manual.
1. Note the leader and the CR statuses.
2. Apply `deploy/bootstrap.yaml` with a new image tag; wait for rollout.

Assert: new ReplicaSet 2/2 and old 0; a new pod holds the lease; existing
`CniInstallation` and `PullThroughCache` stay `Ready` and the leader logs **no**
`checkpointing ledger` line for them (steady state writes nothing extra).
Compare `.status.appliedResources` before and after: unchanged. Trap: the
Deployment status can read `updated=2/ready=2` for a moment before the rollout
starts; use `rollout status`, not a single read.

**CTL-4 Run the controller locally against a cluster (developer workflow)**
Risk `safe`. Status Manual.
`kubectl create namespace platform-system` (the lease lives there), then run
`RUST_LOG=info POD_NAME=<unique> target/debug/platform-controller` with `KUBECONFIG`
set. Assert: it logs `acquired leadership`; on SIGTERM it releases the lease. Use this to
test unreleased code, because the nodes cannot pull an unpublished image. Do not run it
next to the in-cluster controller without accepting that one of them will be standby.

### 4.2 CniInstallation

**CNI-1 Install from a manifest on a CNI-less cluster**
Risk `disruptive`. Status Automated (`tests/integration_talos.rs`, ignored).
Apply a `CniInstallation`; poll `.status.phase`.
Assert: `Ready` with reason `Applied`, `Applied=True`; `appliedResources` matches the
config (for the IPv6 BGP example: Namespace, ServiceAccount, 2 ClusterRoles,
ClusterRoleBinding, RoleBinding, Deployment, APIServer, Goldmane, Installation,
Whisker, IPPool, BGPConfiguration, BGPPeer, 2 more IPPools); every node Ready.
Observed: phase `Ready` about 40 s after apply, nodes Ready about 50 s later.

**CNI-2 Steady-state reconcile is a no-op**
Risk `safe`. Status Manual.
Let a resync pass (300 s) or restart the controller.
Assert: phase stays `Ready`/`Applied`; **zero** `checkpointing ledger` log lines;
`kubectl get ippools.crd.projectcalico.org` lists exactly the declared pools;
`name:phase` of every calico-system pod is unchanged (hash it before and after).

**CNI-3 A render failure is visible and non-destructive**
Risk `disruptive` (briefly `Failed`). Status Partial (mapping unit-tested).
1. `kubectl patch cni default --type merge -p '{"spec":{"calico":{"chartVersion":"v9.9.9"}}}'`
2. After one reconcile, read status; then restore the original version.

Assert: `phase=Failed`, reason `RenderFailed`, message names the missing chart;
`appliedResources` **unchanged** (same count); calico-system pods unchanged (hash);
node stays Ready; restoring returns to `Ready`.

**CNI-4 Node address autodetection by Kubernetes InternalIP**
Risk `disruptive` (restarts calico-node, re-forms BGP). Status Partial.
Set `nodeAddressAutodetectionV6Method: kubernetesInternalIP` and remove the CIDR list.
Assert:
- the operator object: `kubectl get installation default -o jsonpath='{.spec.calicoNetwork.nodeAddressAutodetectionV6}'` equals `{"kubernetes":"NodeInternalIP"}`;
- the DaemonSet env `IP6_AUTODETECTION_METHOD=kubernetes-internal-ip`;
- each `calico-node` logs `Including CIDR information from host interface. CIDR="<its InternalIP>/64"`, and no node ever reports the VIP or a SLAAC address.

Validation check (server-side, persists nothing): `kubectl patch installation default --type merge --dry-run=server -p '<patch>'`
is accepted for `{"kubernetes":"NodeInternalIP"}`. Note the operator's own schema also
accepts `cidrs` and `kubernetes` together, so the controller's rejection
(`InvalidAutodetection`) is the only guard. A live test for that rejection has not been
run (unit-tested only).
Trap: the DaemonSet rolls one pod at a time (`maxUnavailable: 1`); already-unready pods
are replaced first and can stall the roll behind them.

**CNI-5 Calico health gate (reusable assertion)**
Risk `safe`. Status Manual.
Assert: `calico-node` ready equals desired; zero `Failed to connect to typha` in the
last ~1500 log lines of each pod; `v3.projectcalico.org` `Available=True`; zero
restarts. Use this as the gate between the disruptive scenarios below.

### 4.3 PullThroughCache (Spegel)

**PTC-1 Apply the example**
Risk `safe`. Status Automated (ignored `tests/integration_pull_through_cache.rs`) and Manual.
Apply `examples/pull-through-cache.yaml`.
Assert: `Ready`/`Applied` within about 30 s; ledger is Namespace, ServiceAccount,
three Services, DaemonSet; namespace labels `pod-security.kubernetes.io/{enforce,audit,warn}=privileged`;
the init container's `--mirrored-registries` values are URLs; the DaemonSet's
`--mirror-targets` list has **exactly one** entry. Observed: `Ready` in about 10 s.

**PTC-2 Validation failure with a preserved ledger**
Risk `safe`. Status Partial.
Patch `spec.spegel.registries` to `["docker.io"]` on a `Ready` resource.
Assert: `Failed`, reason `InvalidRegistry`, message says URL required; `appliedResources`
**unchanged**; deleting the resource afterwards still removes everything.
Other validation rules (empty list, whitespace `chartVersion`, non-object `helmValues`)
are unit-tested only.

**PTC-3 Wrong chart version is visible**
Risk `safe`. Status Manual.
Apply the example with `chartVersion: "v0.7.4"` (the OCI tag has **no** `v`).
Assert: within one reconcile `Failed`/`RenderFailed`, message contains `not found`;
patching to `0.7.4` logs `checkpointing ledger before applying entries=6` and reaches
`Ready`.

**PTC-4 Partial apply, then delete, leaves nothing behind**
Risk `destructive` to the component. Status Manual.
Apply with `helmValues: {resources: {limits: {memory: bogus}}}` so the API server rejects
the DaemonSet after the earlier objects were applied.
Assert: `Failed`/`ApplyFailed`; ledger lists all 6 planned objects; `ns/spegel` exists
with a ServiceAccount and Services; after `kubectl delete ptc default` the namespace is
gone within about 15 s. This is the regression test for the "ledger written only at the
end" bug.

**PTC-5 Delete and cleanup**
Risk `destructive`. Status Automated (ignored) and Manual.
`kubectl delete ptc default`.
Assert: finalizer clears, everything in `ns/spegel` and the namespace are gone within
30 s (observed 16 s). Then see RES-5 for the fallback behavior.

**PTC-6 Re-create after delete**
Risk `safe`. Status Manual.
Re-apply the example after PTC-5. Assert: `Ready`, DaemonSet rolls to 6/6, `hosts.toml`
rewritten, and a fresh image gives a peer hit (MIR-1).

**PTC-7 Single-node cluster limitation**
Risk `safe`. Status Manual (needs a one-node cluster).
Assert: the Spegel registry container never becomes Ready (startup probe 500, log
`routing table is empty after bootstrapping`); the init container still exits 0; image
pulls on the node keep working (fail-open). Do not expect this scenario on the six-node
cluster.

**PTC-8 IPv6 mirror-target regression (the important one)**
Risk `safe`. Status Partial (unit tests plus an ignored real-chart test).
After PTC-1 on an IPv6-only cluster:
- read `/etc/cri/conf.d/hosts/<registry>/hosts.toml` on a node (`talosctl read`): exactly one `host.` entry, of the form `http://[<node-ip>]:30020`;
- the node's containerd log (`talosctl logs cri`) contains **no** `failed to decode ... hosts.toml`;
- the controller log contains `dropped chart mirror targets ... dropped=1` for chart `0.7.4`.

Cause, so the test fails for the right reason: Spegel brackets only the first
`--mirror-targets` value, and chart 0.7.x adds an unbracketed NodePort target that makes
containerd reject the whole file. Upstream issue: spegel-org/spegel#1539. For chart
`0.8.0-rc.1` the count is expected to be 0 (single target); rendering was checked, a live
run was not.

### 4.4 Mirror behavior (peer-to-peer serving)

All of these read Spegel's counters; see the snippet in section 6. The counter that
matters is `spegel_mirror_requests_total{cache="hit"|"miss",registry="<host>"}`. It does
not exist until the first request, and it is cumulative, so **always take a delta**.

**MIR-1 A second node fetches from a peer**
Risk `safe`. Status Manual (the ignored integration test covers apply and cleanup only, not peer hits).
1. Pick an image no node has (use a fresh tag every run).
2. Pull it pinned to `NODE_A`; then pull it pinned to `NODE_B`.

Assert: `NODE_B` mirror `cache="hit"` delta > 0 and `miss` delta 0; the Pulled event
message on `NODE_B` is not `already present`. Observed (small image): upstream 3.6 s,
peer 0.7 to 2.0 s. Do not assert on speed; see MIR-3.

**MIR-2 All four mirrored registries**
Risk `safe`. Status Manual.
Repeat MIR-1 for `registry.k8s.io`, `quay.io`, `ghcr.io` and `docker.io`, using the
`registry` label to read each counter. Observed image sizes 12 to 71 MB; peer pull was
faster in three of four (`registry.k8s.io` agnhost 52 MB was slower: 12.4 s vs 7.9 s).

**MIR-3 Large multi-layer image**
Risk `safe`. Status Manual.
`docker.io/library/postgres:16` (160 MB compressed, multi-layer). Observed: upstream
33.7 s, peer 26.2 s. Assert success and a hit; treat timing as informational only.

**MIR-4 Concurrent pulls ("thundering herd")**
Risk `safe`. Status Manual.
Start the same fresh image on all six nodes within the same second, then wait for all.
Assert: every pod's Pulled event is present, no failures. Observed (`mariadb:11`,
104 MB): each node 27 to 39 s, wall 46 s, mirror hit +3 / miss +69. Expect a cold herd to
mostly miss; the value is in the second wave.

**MIR-5 Spegel pod killed mid-pull**
Risk `disruptive`. Status Manual.
Variant A: pull on `NODE_A`, then start the same pull on `NODE_B` and delete the Spegel
pod on `NODE_A` (the serving peer) about 4 s in. Variant B: delete the **pulling** node's
own Spegel pod about 4 s in.
Assert: the pull still completes; the killed pod is replaced and Ready within about
15 s. Observed: 158 MB pull finished in 21.7 s (A) and a 296 MB pull in 38.7 s (B), both
about equal to upstream time, so the result is consistent with fallback. The test cannot
tell which path each pull took; assert completion, not the path.

**MIR-6 Cache survives a full restart**
Risk depends on RES-3. Status Manual.
After RES-3, pull an image cached earlier on a node. Assert the event reads
`already present`, and a fresh image still yields a peer hit (observed +8).

**MIR-7 Talos prerequisite present**
Risk `safe`. Status Manual (verified only indirectly).
The nodes need `discard_unpacked_layers = false` in
`/etc/cri/conf.d/20-customization.part`. The scenarios above only pass with it. A direct
check would `talosctl read` that file on every node.

### 4.5 Node addressing and reachability

These diagnose the environment problem that made Typha, BGP and host-to-pod traffic fail.

**NET-1 Source-address selection survey**
Risk `safe`. Status Manual.
In a `calico-node` pod (host network; it has `ip`, no `awk`/`curl`/`python`), per node:
`ip -6 -o addr show dev eth0 scope global`, `cat /proc/sys/net/ipv6/conf/eth0/autoconf`,
`ip -6 route show | grep '^<node-cidr> .*src'`, and `ip -6 route get <peer-ip>` and
`ip -6 route get <remote-pod-ip>`.
Assert: `autoconf=0`; the only global address is the node's own (plus the VIP on the
holder); the `/64` route carries `src` = the node's own address; on-link `src` equals the
node's own address. Observed: pod-bound `src` is the node address on all but the VIP
holder, which prefers the VIP (`/128`), and that still works.
Background: Linux caps the "longest matching prefix" score at each address's own prefix
length, so two `/64` addresses tie and the VIP `/128` wins outright.

**NET-2 Reachability matrix and silent-drop detection**
Risk `safe`. Status Manual.
From each node to a peer's node address: connect to an **unused** port and to the Typha
port (5473). From each node to a pod on another node: connect to the pod's port.
Interpret the exit code: `0` open, `124` timeout, other = refused.
Assert: an unused port is **refused** (fast reset), never a timeout. A timeout on an
unused port means packets are dropped on the path (firewall or security group); a refusal
means the network delivered them. This is the check that separates a real listener
problem from a drop.

**NET-3 Talos machine config check (read-only)**
Risk `safe`. Status Manual.
`talosctl get machineconfig -o yaml` and inspect `machine.sysctls`,
`machine.network.interfaces[].routes` and any `NetworkDefaultActionConfig` /
`NetworkRuleConfig` documents. Also `talosctl get nftableschain` (empty means no Talos
firewall). Assert the expected sysctls and routes are present, and print **only** the
sections you need, since the config contains secrets.

**NET-4 Runtime route experiment (revertible)**
Risk `disruptive`. Status Manual.
In a temporary namespace labelled `pod-security.kubernetes.io/enforce=privileged`, run a
`privileged` `hostNetwork` pod with the calico-node image pinned to one node. Record
`ip -6 route get <peer>` sources, `ip -6 route add <node-cidr> dev eth0 src <node-ip> metric 100`,
re-measure, open a real connection and read its local address from `/proc/net/tcp6`
(decode the hex), then `ip -6 route del` the same route and delete the namespace.
Assert: `src` flips to the node address for every on-link destination and reverts
exactly. Trap: namespace deletion hangs while an aggregated API is unavailable
(see NET-2 and CNI-5).

**NET-5 Packet capture (use with care)**
Risk `safe` but privacy-sensitive. Status Manual.
`talosctl pcap` proved a SYN never reached the destination interface. Older clients take
**raw BPF instructions**, not tcpdump syntax; a failed filter compile silently produced an
**unfiltered** capture of all node traffic once. If you automate this, assert the filter
argument is non-empty, use a client matching the server, cap duration, and delete the
capture afterwards.

### 4.6 Resilience

Gate every one of these with CNI-5 and the Spegel readiness check before starting, and
again after recovery.

**RES-1 Worker node reboot**
Risk `disruptive`. Status Manual.
`talosctl -e <ip> -n <ip> reboot` on a worker, then wait for return.
Assert: node Ready again (observed about 36 s; too fast for Kubernetes to mark it NotReady,
so do not wait for NotReady); calico-node and Spegel back to N/N; only the node's own
global address, `autoconf=0`, `/64` route source correct; `hosts.toml` rewritten; a
peer-cached image pulled on the rebooted node gives a hit (+11 observed). Expect stale
`Error`/`ContainerStatusUnknown` Typha pods left behind; they are not failures.

**RES-2 Control-plane VIP failover**
Risk `disruptive`. Status Manual.
Find the VIP holder (the node with the VIP as a `/128`), reboot it, and poll the API
through the VIP every ~3 s.
Assert: no failed API check longer than your tolerance (observed none); the VIP moves to
another control-plane node; the controller lease moves to a pod on a live node;
`v3.projectcalico.org` stays Available; the new holder can still reach a pod on another
node (it sends from the VIP, which the security group must allow); etcd reports three
members with `errors=false` after the old holder returns (about 100 s).

**RES-3 Full-cluster restart**
Risk `destructive`. Status Manual. Run last.
Issue `reboot --wait=false` to every node at the same moment, each to its own endpoint,
then poll: API up/down, nodes Ready, calico-node, Spegel, controller replicas, both CR
phases.
Assert eventual full recovery with the same running-pod count as before (40 here) and
etcd healthy. Observed timeline: API down at 35 s, back at 108 s, nodes Ready at 131 s,
everything healthy at 171 s. Both CRs briefly report `Failed` while external network
access is not yet available (helm cannot reach the chart hosts), then heal on the next
retry with no action. Assert that heal, not that the brief `Failed` never happens.

**RES-4 Controller leader failover**
Risk `disruptive`. Status Automated (ignored `tests/leader_election.rs`) and observed in
RES-2. Assert exactly one holder at any time and that a standby takes over after the
leader pod is deleted.

**RES-5 Fail-open after `kubectl delete ptc`**
Risk `destructive`. Status Manual.
After PTC-5, pull a fresh image from all four registries on two nodes.
Assert: every pull succeeds; the node's mirror config files remain (the chart's
post-delete hook is not run because the controller renders with `--no-hooks`); the mirror
port now refuses connections; each pull logs one `connection refused` warning in the
node's containerd log and then falls back to upstream within normal pull time. Restore
with PTC-6.

**RES-6 Disk-full node**
Risk `disruptive` (the node gets a DiskPressure taint; nothing is rebooted). Status Manual.
Fill one worker's image filesystem with a privileged host-path pod that writes 1 MiB
chunks (`calico-node` has no `dd`, `df` or `seq`; use `printf -v chunk '%1048576s' ''`
in a loop, and size the fill from the kubelet's own stats, not `df`). Stop at about 2 %
free. Keep the images you care about **in use** by a running pod, because the kubelet's
image GC deletes unused images first (it reclaimed about 3.9 GB of unused images here).
Assert:
- the kubelet reports `DiskPressure=True` and the `node.kubernetes.io/disk-pressure` taint
  (`EvictionThresholdMet`);
- the full node still **serves** peers: a peer pulling an image cached on it succeeds
  (3.6 s, mirror hit +9, miss 0) and its Spegel pod stays Ready;
- a fresh pull **on** the full node fails cleanly (`ErrImagePull`, "failed to extract
  layer" / "failed to record image pull intent"); Spegel and containerd do not crash;
- restarting the Spegel pod on the full node succeeds (about 15 s), the init container
  exits 0, and all four `hosts.toml` files are byte-identical (one host entry each);
- after cleanup the taint clears only after the kubelet's 5-minute transition period, so
  poll for up to 6 minutes; a pull afterwards works (8.3 s, hit +9).
Not covered: a truly 0 % free disk. Always remove the filler files and namespace in a
trap, and gate on the node's free space before and after.

**RES-7 Image over 1 GB**
Risk `safe` (but it consumes about 5.5 GB per node; nodes here have 17 GB disks and a
second pull of another large image can trip RES-6 by accident). Status Manual.
Pull `quay.io/jupyter/scipy-notebook:latest` (1.27 GB compressed, 38 layers) on one
worker, then on a second.
Assert: both pulls succeed; the second registers mirror hits (+20 hit, +9 miss observed).
Do **not** assert a speed-up: upstream took 3 m 54 s and the peer pull 3 m 43 s, because
extraction, not transfer, dominates. Use a bounded wait of at least 10 minutes.

### 4.7 Release and chart compatibility

**REL-1 Tag to published image**
Risk `safe`. Status Manual.
After pushing `vX.Y.Z`: the CI run for the tag succeeds (`build-test`, `docker-build`);
`https://hub.docker.com/v2/repositories/estenrye/platform-controller/tags/X.Y.Z` reports
`tag_status: active` with `linux/amd64`; `deploy/bootstrap.yaml` references `X.Y.Z` (the
semver rule strips the `v`); the two local-registry recipes in `tests/` still rewrite the
image (their `sed` matches any tag). A tag can be pushed before its commit is on `main`;
check both refs with `git ls-remote`.

**REL-2 Chart compatibility**
Risk `safe`. Status Partial.
For each supported Spegel chart version, `helm template` with the controller's values and
assert: the containerd registry config path argument, the mirrored registries, the number
of `--mirror-targets` entries (2 on `0.7.4`, 1 on `0.8.0-rc.1`), and that no
`helm.sh/hook` object is rendered. Add a Calico chart check for whether CRDs ship in the
chart (v3.29 ships them, v3.32 does not).

## 5. Lessons that will break your test code

These each cost a wrong result during the manual runs.

1. **Match events by pod UID, never by name.** Events outlive pods. Reusing a pod name
   returned an old "already present" event and made three test runs meaningless. Use a
   unique pod name per run and filter with `involvedObject.uid`.
2. **Use a fresh image tag for every run**, or the pull says `already present`. Docker
   Hub also rate-limits anonymous pulls (manifest requests count).
3. **Parse durations like `1m3.5s`.** A regex for `[0-9.]+s` silently dropped every pull
   over a minute.
4. **Do not parse `kubectl get pods` columns.** The RESTARTS column becomes `1 (3m ago)`
   after a restart and shifts every later column. Use `-o jsonpath` or `-o json`.
5. **Shell portability.** zsh does not split unquoted variables into words, which turned a
   node list into one bogus node name. Write test scripts in bash or use real arrays.
6. **The Talos client must match the server.** A v1.4.6 client against v1.14 nodes fails
   `patch machineconfig` with `missing kind` (multi-document machine config) and could
   corrupt a config. Pin the client version in the test environment.
7. **Container images are minimal.** `calico-node` has `bash` and `ip` but no `awk`,
   `curl`, `python` or `nc`; Spegel is distroless. Read Spegel's `/metrics` (port 9090 on
   the pod IP) with bash's `/dev/tcp` from a host-network pod, and decode
   `/proc/net/tcp6` hex yourself to see socket source addresses.
8. **Timeouts are not refusals.** Distinguish exit code 124 from a reset (NET-2). A
   timeout to an unused port is a network drop.
9. **Counters are cumulative and created lazily.** Take before/after deltas, and expect a
   metric to be absent before its first event.
10. **Wait on conditions, not sleeps.** `kubectl rollout status`, Ready conditions and
    event presence. A read immediately after `apply` can show stale status.
11. **A reboot can be faster than Kubernetes notices.** Do not wait for NotReady before
    waiting for Ready; assert on boot ID or pod ages instead.
12. **Namespace deletion depends on API discovery.** An unavailable aggregated API
    (`v3.projectcalico.org`) keeps namespaces `Terminating`. Gate on CNI-5 first.
13. **Assert on measurements, not on explanations.** Several plausible causes were
    disproved by a direct measurement (source addresses, refused vs timeout, the
    counters). Automate the measurement so a failure points at the layer that broke.

## 6. Helper snippets

Portable bash. Adapt names; they are illustrations, not a library that exists.

```bash
# Pull an image pinned to a node and report the kubelet's own pull result.
pull_on_node() {  # <node> <image>  -> prints "12.3s 71MB" or "ALREADY-PRESENT" or "FAILED: ..."
  local pod="t-$RANDOM$RANDOM" uid
  kubectl run "$pod" --image="$2" --restart=Never \
    --overrides="{\"spec\":{\"nodeName\":\"$1\"}}" >/dev/null
  uid=$(kubectl get pod "$pod" -o jsonpath='{.metadata.uid}')
  for _ in $(seq 1 120); do
    kubectl get events --field-selector "involvedObject.uid=$uid" -o json |
      grep -q '"reason": "\(Pulled\|Failed\|ErrImagePull\)"' && break
    sleep 3
  done
  kubectl get events --field-selector "involvedObject.uid=$uid" -o json | python3 -c '
import sys, json, re
for e in json.load(sys.stdin)["items"]:
    m = e.get("message", "")
    if e["reason"] == "Pulled":
        if "already present" in m: print("ALREADY-PRESENT")
        else:
            t = re.search(r" in ((?:[0-9]+m)?[0-9.]+s) ", m)
            s = re.search(r"Image size: (\d+)", m)
            print(t.group(1) if t else m[:60], "%dMB" % (int(s.group(1)) // 10**6 if s else 0))
        break
    if e["reason"] in ("Failed", "ErrImagePull"): print("FAILED:", m[:100]); break'
  kubectl delete pod "$pod" --wait=false >/dev/null
}

# Read one Spegel mirror counter from a node's Spegel pod, via a host-network pod.
mirror_counter() {  # <spegel-pod-ip> <hit|miss> <registry> <exec-pod-with-bash>
  kubectl -n calico-system exec "$4" -c calico-node -- bash -c \
    "exec 3<>/dev/tcp/$1/9090; printf 'GET /metrics HTTP/1.0\r\nHost: x\r\n\r\n' >&3; cat <&3" 2>/dev/null |
    grep "^spegel_mirror_requests_total{cache=\"$2\",registry=\"$3\"}" | awk '{print $2}'
}

# TCP probe with a distinguishable result: open | TIMEOUT-dropped | refused.
tcp_probe() {  # <exec-pod> <host> <port>
  kubectl -n calico-system exec "$1" -c calico-node -- bash -c \
    "timeout 4 bash -c 'echo > /dev/tcp/$2/$3' >/dev/null 2>&1; rc=\$?
     case \$rc in 0) echo open;; 124) echo TIMEOUT-dropped;; *) echo refused;; esac"
}
```

## 7. Suggested automation design

- **Tiers.** (1) Existing unit and manifest tests in CI on every push. (2) The ignored
  real-chart tests in a scheduled CI job with network access. (3) A live-cluster suite
  against a dedicated test cluster, run on demand or nightly. The live suite cannot run
  in ordinary CI: it needs a multi-node cluster.
- **Language.** New live tests fit the existing style: Rust integration tests using
  `kube` for reads and assertions, shelling out only for `talosctl`, `helm` and
  in-cluster probes. Bash helpers (section 6) are fine for a first pass.
- **Tags.** Tag every test `safe`, `disruptive` or `destructive`, and let the runner
  select by tag and order by it (section 3). `destructive` tests must restore what they
  remove, and a failed run must leave a documented recovery command.
- **Gates.** Reuse CNI-5 and "Spegel N/N" as pre and post conditions for every
  disruptive test, so a failure names the scenario that broke the cluster.
- **Fresh state.** Generate unique image tags and pod names per run (section 5). Keep a
  list of images to allow for Docker Hub rate limits, or use a pull-through cache in front
  of the test cluster.
- **Reports.** Record the environment table (section 2) with each run. Store measured
  times as trends, not pass/fail thresholds, except for the recovery bounds in RES-1 to
  RES-3, which should have generous limits.
- **Cost.** RES-3 restarts the whole cluster and takes several minutes. Keep it out of
  the default run.

## 8. Not yet tested

- A `CniInstallation` first install that fails partway, then is deleted. The ledger
  mechanism is unit-tested and its `PullThroughCache` equivalent (PTC-4) is verified, but
  the CNI path was only exercised in steady state and render failure.
- The controller-side rejection of `kubernetesInternalIP` with a CIDR list, live.
- Spegel chart `0.8.x` on a live cluster; the chart renders correctly with our values.
- A second VIP failover, or failover while a pull is in progress.
- Multi-architecture images; private registries. (Images over 1 GB: RES-7. Disk
  pressure: RES-6, except a truly 0 % free disk.)
- An upstream registry outage, alone or during a restart: designed but not run. The plan
  is a privileged host-network DaemonSet that drops tcp/443 to anything outside `fd00::/8`
  in the `raw` table (`OUTPUT` for containerd, `PREROUTING -i cali+` for pods such as the
  controller's helm), removes its rules on SIGTERM and expires itself after 45 minutes.
  Assertions: an uncached image fails while a peer-cached one still pulls (compare a tag
  pull with a digest pull, since tag resolution may need upstream); Spegel and controller
  restarts during the outage leave every applied object in place, with both CRs reporting
  `Failed` (`RenderFailed`) and healing after the block is removed.
- BGP session state (`Established` per peer) as an explicit assertion. Node Ready and
  `calico-node` Ready implied it, but it was never asserted directly.
- Controller upgrade while a reconcile is in flight.
- Scale beyond six nodes.
