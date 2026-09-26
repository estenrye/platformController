use kube::CustomResourceExt;

/// Every CRD this controller owns, as the multi-document YAML kept in
/// `deploy/crd.yaml`. Shared by `crdgen` and the test that keeps the file fresh.
pub fn generated_yaml() -> String {
    [
        crate::crd::CniInstallation::crd(),
        crate::pull_through_cache::PullThroughCache::crd(),
    ]
    .iter()
    .map(|crd| serde_yaml::to_string(crd).expect("CRD should serialize to YAML"))
    .collect::<Vec<_>>()
    .join("---\n")
}
