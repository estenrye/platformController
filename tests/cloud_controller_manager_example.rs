use platform_controller::ccm_reconciler;
use platform_controller::cloud_controller_manager::{build_values, CloudControllerManager};
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/cloud-controller-manager.yaml";

fn load() -> CloudControllerManager {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a CloudControllerManager")
}

#[test]
fn example_is_a_single_cloud_controller_manager_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CloudControllerManager");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let manager = load();

    assert_eq!(manager.metadata.name.as_deref(), Some("default"));
    ccm_reconciler::validate("default", &manager.spec).expect("the example must be valid");
}

#[test]
fn example_chart_version_is_the_chart_version_not_the_app_version() {
    // The chart is versioned 2.x; the application it deploys is v1.x. Using the
    // app version (or a `v` prefix) fails with "chart ... not found".
    let version = load().spec.openstack.chart_version;

    assert!(!version.starts_with('v'), "{version}");
    assert!(version.starts_with("2."), "{version}");
}

#[test]
fn example_values_name_the_secret_the_comment_tells_you_to_create() {
    let openstack = load().spec.openstack;
    let values = build_values(&openstack);

    assert_eq!(openstack.cloud_config_secret_ref.name, "cloud-config");
    assert_eq!(values["secret"]["name"], "cloud-config");
    assert_eq!(values["secret"]["create"], false);
}
