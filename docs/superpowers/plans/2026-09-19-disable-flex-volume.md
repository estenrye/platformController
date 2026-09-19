# Disable Calico FlexVolume Driver on Talos Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix a confirmed, live-verified bug where Calico's `calico-node` DaemonSet fails to start on Talos because its FlexVolume driver init container tries to create a directory on Talos's read-only root filesystem.

**Architecture:** One-line addition to the JSON literal `src/helm.rs`'s `build_values` already constructs — set `installation.flexVolumePath: "None"` unconditionally, disabling the legacy FlexVolume driver the `tigera-operator` chart otherwise enables by default.

**Tech Stack:** No new dependencies. Same `serde_json::json!` macro already used throughout `build_values`.

**Spec:** [docs/superpowers/specs/2026-09-19-disable-flex-volume-design.md](../specs/2026-09-19-disable-flex-volume-design.md)

## Global Constraints

- `flexVolumePath` is a top-level field under `installation`, a sibling of `enabled` and `calicoNetwork` — NOT nested under `calicoNetwork`. Verified against the live cluster's actual installed CRD schema (`kubectl explain installation.spec.flexVolumePath`).
- The value must be the literal string `"None"` (capital N) — this is the operator's own sentinel for "disabled," not a JSON `null`.
- Unconditional for every reconcile — no new `CalicoSpec` field, no per-platform branching (there is currently only one platform, `talos-linux`, and it always needs this).

---

## Task 1: Set `flexVolumePath: "None"` in the rendered Helm values

**Files:**
- Modify: `src/helm.rs:22-42` (the `build_values` function body)

**Interfaces:** None — this task doesn't change `build_values`'s signature (`fn build_values(calico: &CalicoSpec) -> serde_json::Value`) or add any new public items. No other file consumes anything new from this task.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/helm.rs` (alongside the existing tests, e.g. right after `translates_bools_to_enabled_disabled_strings`):

```rust
    #[test]
    fn disables_flex_volume_unconditionally() {
        let values = build_values(&sample_spec());

        assert_eq!(values["installation"]["flexVolumePath"], "None");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib helm::tests::disables_flex_volume_unconditionally -- --exact`
Expected: FAIL — `values["installation"]["flexVolumePath"]` is `Value::Null` (the key doesn't exist yet), which does not equal the string `"None"`.

- [ ] **Step 3: Add the field to `build_values`**

In `src/helm.rs`, change the `serde_json::json!` literal at the end of `build_values` from:

```rust
    serde_json::json!({
        "installation": {
            "enabled": true,
            "calicoNetwork": calico_network,
        },
        "apiServer": {
            "enabled": calico.api_server_enabled,
        },
    })
```

to:

```rust
    serde_json::json!({
        "installation": {
            "enabled": true,
            // Talos's root filesystem is read-only, so the legacy FlexVolume
            // driver's init container (which needs to mkdir under
            // /usr/libexec/kubernetes) always crash-loops. FlexVolume is
            // superseded by CSI (already deployed via csi-node-driver), and
            // this controller only ever targets talos-linux today, so
            // disabling it unconditionally is correct, not a platform-specific
            // workaround bolted onto a shared default.
            "flexVolumePath": "None",
            "calicoNetwork": calico_network,
        },
        "apiServer": {
            "enabled": calico.api_server_enabled,
        },
    })
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib helm:: 2>&1 | tail -20`
Expected: all `helm::` tests pass, including the new one.

- [ ] **Step 5: Commit**

```bash
git add src/helm.rs
git commit -m "fix: disable Calico's FlexVolume driver, incompatible with Talos's read-only root"
```

- [ ] **Step 6: Live-verify against the real Talos cluster**

This is the step that actually proves the fix — the unit test only confirms the JSON shape, not that Calico starts on real hardware.

1. Rebuild and republish the image so the fix is actually deployed: `docker build -t platform-controller:latest .` then push it wherever the target cluster pulls from (see `deploy/README.md`/`tests/integration_talos.rs` for the local-registry-mirror approach if the cluster can't reach Docker Hub, or rely on CI's Docker Hub publish if it can).
2. On the live Talos cluster from this bug report (`export KUBECONFIG=~/.kube/kubeconfig`), roll the controller Deployment onto the new image (e.g. `kubectl rollout restart -n platform-system deployment/platform-controller` if using `:latest` with `imagePullPolicy: IfNotPresent` — note the same caching caveat documented in `tests/leader_election.rs`'s header: re-pushing `:latest` does NOT get picked up by nodes that already cached the old image; use a fresh tag if testing against a cluster that already pulled the old `:latest`).
3. Apply `examples/cni-installation.yaml` again if it isn't already present (`kubectl get cniinstallation -A` to check).
4. Watch `kubectl get pods -n calico-system -w` and confirm `calico-node` reaches `Running` (not `Init:CreateContainerError`), followed by `csi-node-driver` and `calico-kube-controllers` also reaching `Running`.
5. Confirm the node's `Ready` condition eventually flips to `True`: `kubectl get nodes`.
6. Record the actual outcome (pass, or what specifically is still broken) — do not claim success without having watched this happen.

---

## Self-Review Notes

- **Spec coverage:** the spec's one Design requirement (add `flexVolumePath: "None"` at the correct nesting level) is Task 1, Step 3. The spec's Testing section's two items (unit test, live verification) are Steps 1-2 and Step 6 respectively.
- **Placeholder scan:** no TBD/TODO; the live-verification step gives concrete commands and explicit "record the actual outcome" instruction rather than a vague "verify it works."
- **Type consistency:** `build_values`'s signature is unchanged; the only new surface is a JSON key inside its existing return value, which the new test asserts directly — nothing for a later task to depend on incorrectly since there is no later task in this plan.
