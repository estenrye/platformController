# Verifying cert-manager on Talos

Manual acceptance for the `CertManagerInstallation` resource. Needs a real
cluster (the Talos-in-Docker setup in `tests/integration_talos.rs` is
enough) with `CniInstallation` already `Ready`. Nothing here has been run
yet: record what you observe under "Findings to record" at the end, the way
`pull-through-cache-verification.md` does.

## 1. Apply and reach Ready

```sh
kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=established --timeout=60s crd/certmanagerinstallations.platform.rye.ninja
kubectl apply -f deploy/bootstrap.yaml
kubectl apply -f examples/cert-manager.yaml
kubectl get certmgr default -o jsonpath='{.status.phase}{"\n"}'      # Ready
kubectl -n cert-manager get pods -o wide
```

Expected: `Ready`; three pods (`cert-manager`, `cert-manager-webhook`,
`cert-manager-cainjector`), all Running.

## 2. Namespace admission under the default (`baseline`) Pod Security Standard

The design assumes cert-manager's pods need no `pod-security.kubernetes.io/*`
labels on their namespace (confirmed by rendering the chart, not by running
it against Talos). Confirm the pods actually started with no admission
rejection and no privilege elevation:

```sh
kubectl get namespace cert-manager -o jsonpath='{.metadata.labels}{"\n"}'   # no pod-security labels
kubectl -n cert-manager get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
kubectl -n cert-manager get pod -l app=cert-manager -o jsonpath='{.items[0].spec.securityContext}{"\n"}'
```

Expected: no `pod-security.kubernetes.io/*` label on the namespace; every pod
`Running`; `runAsNonRoot: true` present. If any pod is stuck `Pending` with an
admission error mentioning Pod Security, this assumption was wrong --
`cert_manager_namespace_object` in `src/cert_manager_reconciler.rs` needs the
same `privileged` labels Calico's and Spegel's namespaces carry, and this
runbook and the design spec both need updating to say so.

## 3. Self-signed ClusterIssuer and Certificate smoke test

This resource does not configure any Issuer; this step is purely to prove
the installed chart actually issues certificates, not part of what
`CertManagerInstallation` manages.

```sh
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: selfsigned-smoketest
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: smoketest
  namespace: default
spec:
  secretName: smoketest-tls
  issuerRef:
    name: selfsigned-smoketest
    kind: ClusterIssuer
  commonName: smoketest.local
  dnsNames:
    - smoketest.local
EOF
kubectl get certificate smoketest -o jsonpath='{.status.conditions[0].type}{"="}{.status.conditions[0].status}{"\n"}'
kubectl get secret smoketest-tls
kubectl delete certificate smoketest
kubectl delete clusterissuer selfsigned-smoketest
```

Expected: `Ready=True` within a few seconds, and `smoketest-tls` exists
holding a TLS Secret. If this fails, the webhook or CA injection isn't
actually working even though `CertManagerInstallation` reports `Ready`
("manifests applied" only, never a health check).

## 4. Delete, and what stays

```sh
kubectl delete certmgr default        # returns once the finalizer clears
kubectl get namespace cert-manager    # NotFound
```

Expected: the chart's objects (including its CRDs) are gone. Any
`Certificate`/`Issuer`/`ClusterIssuer` a person created independently (like
the smoke test above, if not cleaned up) becomes orphaned: nothing renews it
anymore.

## When something goes wrong

- `kubectl get certmgr default -o jsonpath='{.status.phase}{"\n"}{.status.conditions[0].reason}{"\n"}{.status.conditions[0].message}{"\n"}'`
  shows `Failed` with a reason (`InvalidChartVersion`, `InvalidHelmValues`,
  `Unsupported`; `RenderFailed` when helm cannot render the chart, e.g. an
  app-only or un-prefixed chart version; `InvalidManifest`; `ApplyFailed`
  when the API server rejects an object) and the error text.
- The ledger (`status.appliedResources`) is saved before anything is
  applied, so deleting the resource after a failed first install still
  removes everything that was created.

## Findings to record

To fill in from the first live run: whether the namespace-admission
assumption in step 2 held, the smoke-test result in step 3, and anything in
step 4 that differs from "Expected".
