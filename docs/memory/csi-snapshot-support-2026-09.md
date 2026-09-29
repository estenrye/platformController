---
name: csi-snapshot-support-2026-09
description: Sub-project B, split 2026-09-29: the CRD/controller/webhook half shipped as SnapshotController (see [[snapshot-controller-2026-09]]); typed VolumeSnapshotClass support on CsiDriver remains genuinely queued
metadata:
  type: project
---

**Split 2026-09-29.** The CRD/controller/cluster-wide `snapshot-controller`
half of this sub-project shipped as its own component,
[[snapshot-controller-2026-09]] -- see that memory and
`docs/superpowers/specs/2026-09-29-snapshot-controller-design.md` for what
was actually built, including two corrections to the research below (a real
Helm chart does exist; group-snapshot support was included rather than
skipped). The typed-`VolumeSnapshotClass`-on-`CsiDriver` half described
below (naming convention, `storageClasses.additional[]`-shaped
`helmValues`) remains genuinely queued and unbuilt.

Sub-project B of an original request that got decomposed during brainstorming on 2026-09-28. Sub-project A (typed StorageClass customization, [[csi-driver-openstack-cinder-2026-09]]) shipped as `v0.1.6`/`v0.1.7` and is done. This one has **not** been brainstormed to a spec yet -- classification, questions, and design are all still ahead of it. What follows is research and provisional recommendations from that day's conversation, not decisions.

**Goal (as originally asked):** the controller identifies which `snapshot.storage.k8s.io` CRD version matches the running `csi-snapshotter` sidecar, installs the CRDs and the cluster-level `snapshot-controller`, and creates `VolumeSnapshotClass` objects mirroring the StorageClass naming convention (`csi-cinder-ss-delete`/`csi-cinder-ss-retain`), customizable via the `CsiDriver` CR -- now that sub-project A gives `storageClasses.additional[]` a real shape to mirror.

**Research already done (verify it's still current before relying on it -- this is all from one day's research, not re-checked since):**
- `kubernetes-csi/external-snapshotter` has **no official Helm chart** -- raw YAML/kustomize only (`client/config/crd` for CRDs, `deploy/kubernetes/snapshot-controller` for the controller + RBAC). This means the existing `helm.rs` render pipeline can't be reused as-is for this component; a new "fetch raw manifest YAML" path is needed. `crate::manifests::parse_manifests` is chart-agnostic (just parses whatever multi-doc YAML it's given), so the apply/prune/ledger machinery downstream of rendering doesn't care where the YAML came from -- only the render step itself is new.
- Exactly 3 CRDs are needed (`volumesnapshotclasses`, `volumesnapshotcontents`, `volumesnapshots`); the repo also has 3 `volumegroupsnapshot*` CRDs for a separate, newer feature the Cinder chart's sidecar doesn't use -- exclude those (YAGNI).
- The `snapshot-controller` deploy manifest is a `Deployment` (2 replicas, `kube-system`) plus `ServiceAccount`/`ClusterRole`/`ClusterRoleBinding`/`Role`+`RoleBinding` (leader election). All static YAML, no templating/values at all.
- **Real inconsistency found by actually rendering, not assumed:** at git tag `v8.4.0` of that repo, the `Deployment` manifest's own embedded container image is pinned to `snapshot-controller:v8.2.1` -- the git tag and the image tag inside its own YAML don't match. Don't trust "git ref == running image version" as an assumption; if the eventual design needs a specific image version, either pin it explicitly as a typed field (matching this codebase's `chartVersion` convention) or patch the fetched Deployment's image after fetching, decoupling "which commit has the YAML structure" from "which image actually runs."
- Recommendation made (not yet decided with the user): **don't build automatic csi-snapshotter-version-to-CRD-version detection.** The VolumeSnapshot API has been stable at `v1` since Kubernetes 1.20 and hasn't needed a version bump since; auto-detection would be solving a problem that's already effectively static. Pin an explicit version field instead, matching how `chartVersion` already works everywhere else in this codebase -- simpler, more debuggable, no new "detect from a running pod's image tag" mechanism to build and keep working across chart bumps.
- Open architectural question, not resolved: `CsiDriver` is one-CR-per-driver (see [[csi-driver-openstack-cinder-2026-09]]), but the CRDs/`snapshot-controller` are cluster-wide, shared infrastructure -- if a second driver (e.g. `openstack-manila`, `aws-ebs`) is ever added and also wants snapshot support, two `CsiDriver` CRs' finalizers would both think they own the same shared objects, and one's cleanup could delete what the other still needs. Leaning recommendation: make this component's snapshot-infrastructure **apply-only** (create/update, never prune/delete via any single `CsiDriver`'s finalizer) -- sidesteps the shared-ownership problem entirely, matches "cluster addons are sticky once installed" as a defensible operator-facing rule, and avoids over-engineering reference-counting for a multi-driver scenario that doesn't exist yet (only Cinder is built). Not decided with the user -- surface it as an approach choice when this gets brainstormed.

**How to apply:** when this sub-project is picked up, start with `superpowers:brainstorming` (classify architectural: new cluster-wide CRD-and-controller install is a new subsystem, not a bounded change to an existing flow). Re-verify the `external-snapshotter` facts above against whatever version is current then -- this research is a snapshot in time (2026-09-28), not a permanent contract with that upstream project. The naming convention question (`csi-cinder-ss-delete`/`-retain` vs. something else) and the apply-only-vs-full-lifecycle question are both real design forks worth asking the user directly rather than assuming.
