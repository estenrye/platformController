use platform_controller::csi_driver::{build_values, CsiDriver};
use platform_controller::csi_reconciler;
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/csi-driver-openstack-cinder.yaml";

fn load() -> CsiDriver {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a CsiDriver")
}

#[test]
fn example_is_a_single_csi_driver_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CsiDriver");
}

#[test]
fn example_is_named_for_its_driver_and_passes_validation() {
    let driver = load();

    assert_eq!(driver.metadata.name.as_deref(), Some("openstack-cinder"));
    csi_reconciler::validate("openstack-cinder", &driver.spec).expect("the example must be valid");
}

#[test]
fn example_chart_version_is_the_chart_version_not_the_app_version() {
    // The chart is versioned 2.x; the application it deploys is v1.x. Using the
    // app version (or a `v` prefix) fails with "chart ... not found".
    let version = load().spec.openstack_cinder.chart_version;

    assert!(!version.starts_with('v'), "{version}");
    assert!(version.starts_with("2."), "{version}");
}

#[test]
fn example_values_name_the_secret_the_comment_tells_you_to_create() {
    let openstack_cinder = load().spec.openstack_cinder;
    let values = build_values(&openstack_cinder);

    assert_eq!(openstack_cinder.cloud_config_secret_ref.name, "cloud-config");
    assert_eq!(values["secret"]["name"], "cloud-config");
    assert_eq!(values["secret"]["create"], false);
    assert_eq!(values["secret"]["hostMount"], false);
}

#[test]
fn example_marks_csi_cinder_sc_delete_as_the_cluster_default() {
    use platform_controller::csi_driver::DefaultStorageClass;

    let openstack_cinder = load().spec.openstack_cinder;

    assert_eq!(openstack_cinder.storage_classes.default, DefaultStorageClass::Delete);
}
