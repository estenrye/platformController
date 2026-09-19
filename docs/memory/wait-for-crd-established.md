---
name: wait-for-crd-established
description: Fixed a CRD-registration race (Missing Kind discovery error) by waiting for Established before applying dependent objects; established that every networked poll in this codebase must use tokio::time::timeout_at, not a bare await
metadata:
  type: project
---

A live Talos cluster hit `failed to discover API resource for operator.tigera.io/v1/Installation: Missing Kind` on first-ever install: the reconciler applied a CRD via server-side apply, then immediately tried to apply the custom resource it defines, before the API server had finished registering the CRD's REST endpoint. Self-healing (via `error_policy`'s 30s retry) but a real gap, not just an accepted risk.

**Fix:** `wait_for_crd_established` in `src/apply.rs` polls the CRD's `status.conditions` for `type: Established, status: "True"` (10s timeout, 200ms interval) before `reconciler.rs`'s apply loop continues past any object whose kind is `CustomResourceDefinition`. Generic on `kind`, not hardcoded to any one operator's CRDs.

**Non-obvious lesson carried forward from the final whole-branch review:** the first draft's polling loop bounded the *sleep* between attempts but not the `api.get()` call itself. kube-client 4.2.0 defaults to `read_timeout: None` with internal retries — a wedged connection to the apiserver can leave a bare `.await` on an API call pending indefinitely, silently defeating any timeout value the surrounding loop thinks it's enforcing. `src/leader.rs` already guards every one of its networked calls this way with `tokio::time::timeout_at(deadline, future)`; this fix had to be corrected to match. **Any future networked polling loop added to this codebase must wrap the actual API call in `timeout_at`, not just gate the loop's sleep/deadline check around it** — this is now the second time this exact pattern had to be applied (see [[calico-talos-mvp-2026-09]] and the leader-election work), so treat it as a hard rule, not a one-off fix.

**Also surfaced, not yet acted on:** [[rbac-cluster-admin-tradeoff]]'s permission ledger didn't originally record that this controller now does a cluster-wide `get` (poll) on `apiextensions.k8s.io/customresourcedefinitions`, distinct from the create/patch it already needed — now fixed in that ledger. This matters because a future RBAC scope-down that grants create/patch but omits `get` would produce a silent, RBAC-shaped failure that looks identical to a slow apiserver (CRD applies fine, then spins for 10s and times out) — no 403 anywhere in the logs to point at the real cause.

**Deliberately not fixed here** (parked with rulings in this plan's SDD ledger, since resolving them would exceed this fix's scope): a CRD that can *never* establish (name collision, a `Terminating` predecessor) now halts the whole apply loop at that CRD, where previously the loop would still apply everything before the one object that actually depended on it — a diagnosability/blast-radius regression, not a new unrecoverable state. `status.phase` also stays stale on any apply-loop failure (including this new one), but that's pre-existing behavior for every failure in that loop, not something this fix introduced.
