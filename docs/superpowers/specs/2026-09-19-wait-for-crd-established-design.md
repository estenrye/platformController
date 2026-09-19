# Wait for CRD Established Before Applying Dependent Objects

Status: Approved for planning
Date: 2026-09-19

## Purpose

On a real Talos cluster, the first-ever reconcile of a fresh `CniInstallation` logged:

```
ERROR platform_controller: reconcile failed error=reconciler for object CniInstallation.v1alpha1.platform.rye.ninja/default failed:
failed to discover API resource for operator.tigera.io/v1/Installation: Error from discovery: Missing Kind: GroupVersionKind { group: "operator.tigera.io", version: "v1", kind: "Installation" }
```

The reconcile had, moments earlier in the *same pass*, applied the `installations.operator.tigera.io` CRD via server-side apply, then continued applying the rest of the sorted object list — including the `Installation` custom resource itself — without waiting for the API server to actually finish registering the new CRD (`status.conditions[type=Established] = True`). `kube::discovery::oneshot::pinned_kind` queried discovery before the CRD's REST endpoint was live, so the whole reconcile failed with an error, and only succeeded on the next attempt 30 seconds later (`error_policy`'s retry).

This is a self-healing bug — this project's own earlier reviews predicted almost exactly this failure mode and flagged it as an accepted risk — but it is a real gap, not just documented-and-ignored: it produces a scary `ERROR` log and a guaranteed ~30-second delay on every fresh install, and the self-healing is incidental (it depends on the periodic retry cadence happening to be long enough), not something the code actually waits for. This fix makes the wait explicit.

## Non-goals

- General retry/backoff tuning beyond this one gap. `error_policy`'s existing 30s requeue on any other kind of failure is unaffected.
- Waiting for anything other than `CustomResourceDefinition` Established status — this fix closes the one confirmed race (CRD registration lag), not a general "wait for eventual consistency everywhere" mechanism.
- Changing the object apply order (`apply_rank` in `src/manifests.rs`) — CRDs already sort before everything that could depend on them (rank 1 vs. rank 5 default). The gap is that nothing waits between them, not that the order is wrong.

## Design

**New function** in `src/apply.rs`: `wait_for_crd_established(client: &kube::Client, name: &str, timeout: Duration) -> Result<(), ApplyError>`. Polls `kube::Api::<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition>::all(client)` for the named CRD, checking `status.conditions` for an entry with `type_ == "Established"` and `status == "True"`, with a short sleep between polls. Returns a new `ApplyError` variant if the timeout elapses before that condition is observed.

**Integration** in `src/reconciler.rs`'s apply loop: after each `apply_object` call, check the returned `AppliedResourceRef.kind` — if it's `"CustomResourceDefinition"`, call `wait_for_crd_established` (10-second timeout, matching the sort of margin already used elsewhere in this codebase for bounded waits) before continuing to the next object in the sorted list. This uses data `apply_object` already computes (`AppliedResourceRef`), so no extra lookup of the original `DynamicObject` is needed.

This closes the gap for every CRD the chart renders (not just `operator.tigera.io`'s), since the check is generic on `kind == "CustomResourceDefinition"`, not hardcoded to the one CRD that happened to trigger the bug report.

## Testing

- **Unit test**: `wait_for_crd_established`'s core "is this CRD Established" check should be pulled out as a small pure/near-pure helper (e.g. `fn is_established(crd: &CustomResourceDefinition) -> bool`) that can be tested with hand-built `CustomResourceDefinition` values (no cluster) covering: no conditions at all, an Established condition with `status: "False"`, an Established condition with `status: "True"`, and other unrelated condition types present alongside Established. The actual polling loop around it is networked and not unit-tested, matching this codebase's established pattern for thin networked wrappers (e.g. `apply_object`, `helm::render`).
- **Live verification**: re-run against a live Talos cluster starting from a clean state (no `operator.tigera.io` CRDs yet installed) and confirm the first-ever reconcile succeeds without the discovery error — no `ERROR` log, no 30-second delay, `status.phase` reaches `Ready` on the first attempt.
