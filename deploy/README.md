# Deploying the platform controller

Apply in this order:

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/cniinstallations.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
```

`crd.yaml` must be applied — and Established — first. `bootstrap.yaml` contains a
`CniInstallation` custom resource, and the API server rejects a custom resource
whose kind is not yet registered (`no matches for kind "CniInstallation"`).
Registration is asynchronous: the CRD can exist while its API endpoint is not yet
serving, so waiting on `condition=established` (rather than just applying the two
files back to back) is what makes the second apply reliable.

`bootstrap.yaml` uses `image: platform-controller:latest` with
`imagePullPolicy: IfNotPresent`, so a locally built image is used as-is with no
registry. Build it on every node (or push to a registry the cluster can reach)
before applying:

```sh
docker build -t platform-controller:latest .
```

Regenerate `crd.yaml` after any change to the CRD types:

```sh
cargo run --bin crdgen > deploy/crd.yaml
```
