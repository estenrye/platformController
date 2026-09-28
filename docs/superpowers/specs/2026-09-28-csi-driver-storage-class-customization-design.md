# CsiDriver: Typed StorageClass Customization

Status: Draft, awaiting review
Date: 2026-09-28

## Purpose

`CsiDriver`'s `openstack-cinder-csi` support (shipped earlier today) renders exactly two fixed StorageClasses (`csi-cinder-sc-delete`, `csi-cinder-sc-retain`) with no typed control over their `parameters`, and no way to add further StorageClasses except a raw `helmValues.storageClass.custom` YAML string. Live-testing the driver against a real OpenStack cloud (see `docs/runbooks/csi-driver-openstack-cinder-verification.md`) hit exactly this gap: the cluster's Cinder availability zone didn't match the zone the topology-aware provisioner derives by default, and the only way to work around it was a hand-written, out-of-band StorageClass. This spec adds typed `parameters` to the two built-in StorageClasses and a typed list of additional StorageClasses, so cases like the AZ mismatch — and others, like a volume-type-specific class — are expressible directly on the `CsiDriver` CR.

This is the first of two sub-projects split out of one larger request: this spec covers StorageClass customization only. A follow-up spec will add cluster-wide CSI VolumeSnapshot support (CRDs, `snapshot-controller`, and `VolumeSnapshotClass` objects mirroring whatever StorageClasses exist after this spec ships) — deliberately sequenced after this one, since the snapshot classes' naming and count depend on the final StorageClass shape here.

## Non-goals

- CSI VolumeSnapshot support (CRDs, `snapshot-controller`, `VolumeSnapshotClass`). Follow-up spec.
- Typed control over anything on a StorageClass besides `parameters`, `reclaimPolicy` (additional entries only) and `isDefault`. `allowVolumeExpansion`, `volumeBindingMode` and any other field stay at the chart's own defaults, reachable only through `helmValues` if ever needed.
- Validating `parameters`' contents against what Cinder actually accepts (e.g. that an `availability` value names a real AZ). The controller has never read from or validated against the live cloud; this doesn't change that.
- A generic multi-driver mechanism. This spec only changes `OpenstackCinderSpec`; when a second driver is built, its own spec decides whether it needs the same shape.
- Migrating the one live `CsiDriver` CR on the test cluster. The CRD shipped hours before this spec, with a single instance; the runbook step that applies the new example after this change is enough.

## Design

### API

`OpenstackCinderSpec.defaultStorageClass` (a bare field) is replaced by a nested `storageClasses` block. This is a breaking reshape, not an additive change — acceptable given the CRD's age (Non-goals). New shape:

```yaml
apiVersion: platform.rye.ninja/v1alpha1
kind: CsiDriver
metadata:
  name: openstack-cinder
spec:
  platformKind: talos-linux
  driver: openstackCinder
  openstackCinder:
    chartVersion: "2.36.5"
    cloudConfigSecretRef:
      name: cloud-config
    storageClasses:
      default: delete                # enum: delete | retain | none (unchanged values, new location)
      delete:
        parameters:                  # NEW: passed verbatim to storageClass.delete.parameters
          availability: nova
      retain:
        parameters: {}
      additional:                    # NEW: arbitrary extra StorageClasses
        - name: csi-cinder-sc-az1
          reclaimPolicy: Delete      # enum: Delete | Retain
          parameters:
            availability: az1
          isDefault: false
    helmValues: {}
```

- `storageClasses.default`, `.delete`, `.retain` are all optional (default `none`/empty parameters/empty list respectively), so a spec that only sets `chartVersion` and `cloudConfigSecretRef` still deserializes, matching every other optional block in this CRD.
- `storageClasses.delete.parameters` / `.retain.parameters` are `map[string]string`, merged unconditionally into `storageClass.delete.parameters` / `storageClass.retain.parameters` — typed values, so the usual "typed wins over `helmValues`" rule applies without change.
- `storageClasses.additional[]`: `name` (required, DNS-1123 subdomain, reused validator), `reclaimPolicy` (required, `Delete` or `Retain` — capitalized, unlike this CRD's other enums, because the value is copied verbatim into the rendered `StorageClass`'s own `reclaimPolicy` field, which the Kubernetes API itself spells that way), `parameters` (optional map), `isDefault` (optional, default `false`).
- `storageClasses.default` keeps its current three-value enum — it does not grow a fourth "name of a custom entry" option. Making an additional StorageClass the cluster default is done through that entry's own `isDefault: true`, not through `default`.

### Validation

Rejected with `phase: Failed` and a `reason`, added to the existing list in `csi_driver::CsiSpecError`:

- an `additional[].name` is empty or not a valid Kubernetes object name (DNS-1123 subdomain) — reason `InvalidStorageClassName` (a distinct reason from `InvalidSecretRef`, so the status message doesn't conflate a bad StorageClass name with a bad Secret name)
- an `additional[].name` collides with the reserved built-in names `csi-cinder-sc-delete` or `csi-cinder-sc-retain` — reason `ReservedStorageClassName`
- two entries in `additional[]` share the same `name` — reason `DuplicateStorageClassName`
- **at most one default:** `storageClasses.default != none` and any `additional[].isDefault == true`, or more than one `additional[].isDefault == true` — reason `AmbiguousDefaultStorageClass`

No new rejection for `helmValues` setting `storage.custom` — see Non-goals-adjacent note under Values builder below: it is unconditionally overwritten, the same "typed always wins" behavior every other typed field in this CR already has, not a new special case worth its own error type.

### Values builder

`build_values` gains two responsibilities beyond the existing `secret.*`/`storageClass.{delete,retain}.isDefault` overlay:

1. **Parameters.** `storageClass.delete.parameters` and `storageClass.retain.parameters` are set from the typed maps (empty map when unset, matching the chart's own default).
2. **Additional StorageClasses.** Each `additional[]` entry is rendered into a real `StorageClass` YAML document:
   ```yaml
   apiVersion: storage.k8s.io/v1
   kind: StorageClass
   metadata:
     name: <entry.name>
     annotations:
       storageclass.kubernetes.io/is-default-class: "true"   # only when entry.isDefault
   provisioner: cinder.csi.openstack.org
   reclaimPolicy: <entry.reclaimPolicy>
   parameters: <entry.parameters>
   ```
   joined with `---\n` (empty string when `additional` is empty), and set as `storageClass.custom` — the chart's own documented raw-YAML extension point (confirmed from the chart's commented example values, which pairs a custom `StorageClass` and a `VolumeSnapshotClass` in exactly this string form). This is set unconditionally, exactly like every other typed field in `build_values`, so a `helmValues.storageClass.custom` passthrough is always overwritten rather than merged or rejected — `additional[]` becomes the one source of truth for extra StorageClasses once this ships.

No changes to Reconcile, Cleanup, or the reconciler's control flow: this spec only changes what goes into the values passed to the existing `helm template` call.

## Testing

- **Unit:** each new validation rejection (bad name, reserved name, duplicate name, ambiguous default in both directions — `default` conflicting with an `additional[]` entry, and two `additional[]` entries both `isDefault: true`); `build_values` sets `storageClass.{delete,retain}.parameters`; `build_values`'s `storageClass.custom` string, parsed back with `serde_yaml`, yields the expected `StorageClass` object(s) with correct `provisioner`, `reclaimPolicy`, `parameters` and the default annotation only where expected; an empty `additional[]` produces an empty `storageClass.custom` string, not an absent key (so it still unconditionally overrides a `helmValues` attempt).
- **Real-chart (ignored, needs network):** extend the existing test to pass one `additional[]` entry through `build_values` and confirm the rendered object list contains a matching `StorageClass`, alongside the existing delete/retain assertions.
- **Example:** `examples/csi-driver-openstack-cinder.yaml` and `tests/csi_driver_example.rs` updated for the reshaped `storageClasses` block; the `defaultStorageClass: delete` comment moves to `storageClasses.default: delete` with the same explanatory comment.

## Verification status

Not yet implemented.
