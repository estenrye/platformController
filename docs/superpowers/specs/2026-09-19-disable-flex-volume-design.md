# Disable Calico's FlexVolume Driver on Talos

Status: Approved for planning
Date: 2026-09-19

## Purpose

A real install on a live Talos Linux single-node cluster failed to bring Calico up: `calico-node`'s `flexvol-driver` init container crash-looped with `mkdir /usr/libexec/kubernetes: read-only file system`. The controller's own reconcile reported `status.phase = Ready` (manifests applied successfully), but Calico itself never actually started — `calico-node` stuck in `Init:CreateContainerError`, which cascaded into `csi-node-driver` (`NetworkNotReady`, since no CNI plugin ever initialized) and `calico-kube-controllers` (`Pending`).

Root cause: the `tigera-operator` chart's default `Installation` values enable the legacy FlexVolume driver, which needs to create a directory under `/usr/libexec/kubernetes/...`. Talos Linux's root filesystem is read-only by design (immutability is a core Talos property), so that `mkdir` always fails. Verified against the live cluster's actual installed CRD schema (`kubectl explain installation.spec.flexVolumePath`): setting `flexVolumePath: "None"` on the `Installation` resource disables the FlexVolume driver entirely, which is exactly what's needed — this controller only ever targets `platformKind: talos-linux` today, and FlexVolume (superseded by CSI, which this chart already deploys via `csi-node-driver`) has no reason to be enabled.

This is the fix that makes the MVP actually produce a working Calico install, rather than a controller that reports `Ready` while the cluster's CNI never comes up.

## Non-goals

- Making `flexVolumePath` configurable per-`CniInstallation` (e.g. via a new CRD field). There is currently exactly one supported platform (`talos-linux`), which always needs FlexVolume disabled — a config knob for a value that's always the same is unnecessary until a second platform with different needs exists.
- Any other Talos-specific chart-value adjustments beyond this one field. This spec fixes the one confirmed, live-verified blocker; it does not attempt to preemptively audit the rest of the chart's defaults for other possible Talos incompatibilities.
- Gating `status.phase = Ready` on actual Calico pod health (that's a separate, already-tracked spec gap — see the "Ready doesn't mean ready" discussion from this investigation, deliberately out of scope for this fix).

## Design

**Change:** `src/helm.rs`'s `build_values` adds a top-level `flexVolumePath: "None"` key under `installation` (a sibling of `calicoNetwork` and `enabled`, per the real `Installation` CRD schema — `flexVolumePath` is not nested under `calicoNetwork`), unconditionally, for every reconcile.

```json
{
  "installation": {
    "enabled": true,
    "flexVolumePath": "None",
    "calicoNetwork": { ... }
  },
  "apiServer": { ... }
}
```

No new function, no new field on `CalicoSpec`, no new CRD schema change — this is a one-line addition to the JSON literal `build_values` already constructs, matching the "unconditional, platform-implied default" pattern already used elsewhere in that function (e.g. `installation.enabled: true` is likewise not derived from any `CalicoSpec` field).

## Testing

- **Unit test**: extend `src/helm.rs`'s existing test module with an assertion that `build_values(&sample_spec())["installation"]["flexVolumePath"] == "None"` — a one-line addition alongside the existing assertions on `installation.calicoNetwork.bgp` etc.
- **Live verification**: re-apply the example `CniInstallation` against the same live Talos cluster this bug was found on, and confirm `calico-node` reaches `Running` (not `Init:CreateContainerError`), `csi-node-driver` reaches `Running`, and the node's `Ready` condition eventually flips to `True`. This is the actual proof the fix works — a passing unit test alone doesn't confirm Calico starts on real Talos hardware.
