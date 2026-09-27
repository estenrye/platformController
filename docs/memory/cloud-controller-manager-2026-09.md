---
name: cloud-controller-manager-2026-09
description: CloudControllerManager (OpenStack) slice, 2026-09-26 - third CRD beside CniInstallation and PullThroughCache; chart facts from a real render; nothing live-verified yet
metadata:
  type: project
---

`CloudControllerManager` (cluster-scoped singleton `default`, shortname `ccm`) installs cloud-provider-openstack on self-hosted Talos clusters running on OpenStack VMs. Spec: `docs/superpowers/specs/2026-09-26-cloud-controller-manager-design.md`; plan: `docs/superpowers/plans/2026-09-26-cloud-controller-manager.md`; live acceptance: `docs/runbooks/cloud-controller-manager-verification.md`. It is a third parallel component (own reconciler, third `Controller` in `main.rs`, same leader lease). The finalizer and status glue is now duplicated three times: that is the evidence for extracting a shared component framework, deliberately not done in this slice ([[pull-through-cache-2026-09]] deferred it until a third component). AWS, GCP, Azure and OCI providers, Cinder/Manila CSI, typed `cloud.conf` fields and a Secret existence check are deliberately not built.

**Non-obvious facts (from rendering the real chart, not assumed):**
- Chart `openstack-cloud-controller-manager` `2.36.5` (app `v1.36.0`) from the classic repo `https://kubernetes.github.io/cloud-provider-openstack`; `chartVersion` is the CHART version (2.x), not the app version, and has no `v` prefix. The existing `--repo` render path is reused unchanged.
- `secret.create=false` renders NO Secret object. The DaemonSet mounts the Secret by name and reads `/etc/config/cloud.conf`, so the key inside the user's Secret must be `cloud.conf`; the chart's `secret-reader` Role is scoped by `resourceNames` to the same name.
- The chart's default `extraVolumes` hostPath-mount `/etc/kubernetes/pki` and the flexvolume dir; the controller always overrides both lists with `[]` (Talos provides neither, the CCM uses in-cluster config). With the override the only volume is the Secret.
- The DaemonSet is `hostNetwork` and tolerates `node.cloudprovider.kubernetes.io/uninitialized`, so it needs no CNI. It targets `node-role.kubernetes.io/control-plane` nodes and lives in `kube-system` (no namespace is synthesized; Talos exempts it from Pod Security).
- The release name `openstack-ccm` is in the DaemonSet's immutable selector (`release: openstack-ccm`); never change it.
- **The controller's own Deployment had to gain a toleration for the `uninitialized` taint** (`deploy/bootstrap.yaml`): on a cluster whose kubelets use `--cloud-provider=external` it could otherwise never schedule, and nothing could install the CCM.
- The Talos node prerequisite (`cluster.externalCloudProvider.enabled: true`) cannot be applied or checked by the controller; `Ready` means manifests applied only. A missing cloud-config Secret shows as `Ready` with a pod stuck in `ContainerCreating`.

**Not verified:** everything on a live cluster. Open items recorded in the runbook: node initialization, node addresses (and the interaction with `nodeAddressAutodetectionV6Method: kubernetesInternalIP`), LoadBalancer Services, whether the chart-default `route` controller conflicts with Calico (if so, override `enabledControllers` without `route`), the controller pod scheduling under the new toleration, and the `extraVolumes: []` override on Talos.

**How to apply:** when bumping the chart version, re-run the ignored real-chart test `helm::tests::openstack_ccm_chart_renders_the_shape_the_spec_relies_on` and re-check the Secret mount, the volumes and the hook rendering.
