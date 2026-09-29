# Verifying cert-manager on Talos

Manual acceptance for the `CertManagerInstallation` resource. Needs a real
cluster (the Talos-in-Docker setup in `tests/integration_talos.rs` is
enough) with `CniInstallation` already `Ready`. Steps 1-4 were all run and
passed on a live cluster on 2026-09-29; see "Findings to record" at the end
and `docs/memory/cert-manager-2026-09.md` for the full write-up.

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
labels on their namespace -- confirmed both by rendering the chart and, as of
2026-09-29, by running it against a live Talos cluster. Confirm the pods
actually started with no admission rejection and no privilege elevation:

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

Expected: the chart's objects, **including its CRDs**, are gone. Kubernetes
deletes every instance of a kind when its CRD is deleted, so this destroys
**every** `Certificate`/`Issuer`/`ClusterIssuer`/`CertificateRequest`/
`Order`/`Challenge` in the cluster, not just the ones from the smoke test
above -- confirm the smoke test's own resources were already deleted at the
end of step 3, but understand that this step is destructive to any other
cert-manager resources on the cluster too, not just leftovers.

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

Live-verified 2026-09-29 on a 6-node Talos cluster (3 control-plane, 3
worker; controller `0.1.8`, chart `v1.16.2`), alongside the other three
components which stayed `Ready` throughout. All four steps matched
"Expected" exactly, with no deviations:

- Step 1: `Ready` in ~9s; all three pods Running.
- Step 2: no `pod-security.kubernetes.io/*` label appeared on the namespace;
  every pod Running with `runAsNonRoot: true`. The design's assumption held.
- Step 3: `Ready=True` on the first poll; `smoketest-tls` held real
  certificate data. The webhook and CA injection are genuinely functional.
- Step 4: the delete returned once the finalizer cleared, the namespace
  fully terminated, and all six `cert-manager.io` CRDs were gone -- the
  cascade-delete behavior documented in this runbook and `deploy/README.md`
  is real, not theoretical.

Full write-up: `docs/memory/cert-manager-2026-09.md`.
