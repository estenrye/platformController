use platform_controller::cert_manager::{build_values, CertManagerInstallation};
use platform_controller::cert_manager_reconciler;
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/cert-manager.yaml";

fn load() -> CertManagerInstallation {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a CertManagerInstallation")
}

#[test]
fn example_is_a_single_cert_manager_installation_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CertManagerInstallation");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    cert_manager_reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_chart_version_carries_the_v_prefix() {
    // Unlike the OpenStack charts (no "v" prefix on the chart version), the
    // cert-manager chart and app versions track together and both use "v".
    let version = load().spec.chart_version;

    assert!(version.starts_with('v'), "{version}");
}

#[test]
fn example_values_always_enable_crds() {
    let values = build_values(&load().spec);

    assert_eq!(values["crds"]["enabled"], true);
}
