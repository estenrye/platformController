# Verifying the OpenStack cloud controller manager on Talos

Manual acceptance for the `CloudControllerManager` resource. Needs a real
OpenStack cloud you can boot Talos VMs in, credentials for it, and at least one
control-plane and one worker node. Nothing here has been run yet: record what you
observe under "Findings to record" at the end, the way
`pull-through-cache-verification.md` does.

## 0. Node prerequisite (once, at cluster creation)

Every node's kubelet must run with `--cloud-provider=external`, and the control-plane
components must be configured for an external cloud provider. Talos exposes this as
a machine-config switch. Patch the machine config of every node (control plane and
workers) at creation:

```yaml
# ccm-talos-patch.yaml
cluster:
  externalCloudProvider:
    enabled: true
```

```sh
talosctl gen config <cluster> https://<endpoint>:6443 --config-patch @ccm-talos-patch.yaml
```

On a running cluster use `talosctl patch machineconfig --nodes <ip> --patch @ccm-talos-patch.yaml`
instead; kubelet needs a restart, which Talos reports. Confirm the result on a node:

```sh
talosctl -n <node-ip> get kubeletspecs -o yaml | grep -i cloud-provider    # external
```

Record the Talos version and the exact patch that worked. The controller cannot
apply or check this; it reports only that its own manifests were applied.

From now on each node registers with the taint
`node.cloudprovider.kubernetes.io/uninitialized:NoSchedule` and keeps it until the
CCM initializes it:

```sh
kubectl get nodes -o custom-columns=NAME:.metadata.name,TAINTS:.spec.taints[*].key
```

## 1. The controller schedules under the new toleration

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/pullthroughcaches.platform.rye.ninja
kubectl wait --for=condition=established --timeout=60s crd/cloudcontrollermanagers.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl -n platform-system get pods -o wide
```

Expected: both `platform-controller` pods are scheduled and Running on nodes that
still carry the `uninitialized` taint. Before the toleration in `deploy/bootstrap.yaml`
they would sit `Pending` (`untolerated taint`). Record which nodes they landed on.

## 2. Create the credentials Secret, then apply

Create the cloud config as `cloud.conf` (the key matters). A minimal example; use
your own auth URL, an application credential and region, and add `[Networking]` /
`[LoadBalancer]` sections as needed (see the cloud-provider-openstack docs):

```ini
[Global]
auth-url=https://keystone.example.com:5000/v3
application-credential-id=<id>
application-credential-secret=<secret>
region=RegionOne

[LoadBalancer]
use-octavia=true
floating-network-id=<external-network-uuid>
subnet-id=<subnet-uuid>
```

```sh
kubectl -n kube-system create secret generic cloud-config --from-file=cloud.conf=./cloud.conf
kubectl apply -f examples/cloud-controller-manager.yaml
kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n kube-system get ds openstack-cloud-controller-manager -o wide
kubectl -n kube-system get pods -l app=openstack-cloud-controller-manager -o wide
```

Expected: `Ready`; one pod per control-plane node, Running. `Ready` means the
manifests were applied, not that the CCM is healthy: the pod is the real signal.
Check `kubectl -n kube-system logs <pod>` for errors reaching Keystone.

This also verifies the `extraVolumes: []` override: the pod starts on Talos with
no hostPath mounts (`kubectl -n kube-system get pod <pod> -o yaml | grep -c hostPath` is `0`).

## 3. Node initialization

```sh
kubectl get nodes -o custom-columns=NAME:.metadata.name,PROVIDERID:.spec.providerID,TAINTS:.spec.taints[*].key
```

Expected: every node has a `providerID` of the form `openstack:///<instance-uuid>` and
no `uninitialized` taint. Record the time from applying the CR to the taint clearing.

## 4. Node addresses and the IPv6 interaction

```sh
kubectl get nodes -o wide
kubectl get node <node> -o jsonpath='{.status.addresses}{"\n"}'
```

The CCM can rewrite each node's addresses from Neutron. Compare the `InternalIP` /
`ExternalIP` values before and after step 2 (`kubectl get nodes -o wide` beforehand).
If the cluster uses `nodeAddressAutodetectionV6Method: kubernetesInternalIP` on
`CniInstallation`, check whether Calico's view of node addresses changed
(`kubectl get nodes.projectcalico.org -o yaml` or the `calico-node` logs) and record
what you find. Nothing is designed around this yet.

## 5. LoadBalancer Service

```sh
kubectl create deployment web --image=registry.k8s.io/pause:3.10 --replicas=1
kubectl expose deployment web --port=80 --type=LoadBalancer
kubectl get svc web -w         # EXTERNAL-IP moves from <pending> to an address
```

Expected: an external address, and a matching load balancer in Octavia. Delete the
Service afterwards and confirm the load balancer is removed
(`openstack loadbalancer list`).

## 6. The `route` controller under Calico

The chart enables the `route` controller by default. Look for it doing anything to
Neutron routes or fighting Calico:

```sh
kubectl -n kube-system logs -l app=openstack-cloud-controller-manager | grep -i route
openstack router show <router> -c routes
```

If it programs routes that conflict with Calico's pod routing, record it: the fix is an
unconditional `enabledControllers` override without `route` (spec: "The `route`
controller"), and the spec and `build_values` change with it.

## 7. Missing Secret

```sh
kubectl -n kube-system delete secret cloud-config
kubectl -n kube-system delete pod -l app=openstack-cloud-controller-manager
kubectl -n kube-system get pods -l app=openstack-cloud-controller-manager     # ContainerCreating
kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}'                   # still Ready
```

Expected (and by design): the pod cannot mount the Secret and stays
`ContainerCreating`, while status still says `Ready`. Recreate the Secret and the
pod starts. This is the failure mode to check first when nodes stay tainted.

## 8. Delete, and what stays

```sh
kubectl delete ccm default        # returns once the finalizer clears
kubectl -n kube-system get ds openstack-cloud-controller-manager          # NotFound
kubectl get nodes -o custom-columns=NAME:.metadata.name,PROVIDERID:.spec.providerID
```

Expected: the chart's objects are gone; `providerID`s remain (node initialization is
not undone). `kube-system` is untouched. Existing cloud load balancers remain, and
`LoadBalancer` Services stop being reconciled until the CR is re-applied.

## When something goes wrong

Failures appear on the resource, not only in the controller's logs:

- `kubectl get ccm default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidSecretRef`,
  `InvalidHelmValues`, `Unsupported`; `RenderFailed` when helm cannot render the
  chart, e.g. the app version `v1.36.0` used as the chart version; `InvalidManifest`;
  `ApplyFailed` when the API server rejects an object) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is applied, so
  deleting the resource after a failed first install still removes everything that
  was created.

## Findings to record

To fill in from the first live run: the Talos version and patch that worked (step 0),
where the controller pods scheduled (step 1), the time to node initialization (step
3), the address comparison and Calico's behavior (step 4), the LoadBalancer result
(step 5), what the `route` controller did (step 6), and anything in step 8 that
differs from "Expected".
