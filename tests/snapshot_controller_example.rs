use platform_controller::manifests::parse_manifests;
use platform_controller::snapshot_controller::{build_values, SnapshotController};
use platform_controller::snapshot_controller_reconciler;

const EXAMPLE: &str = "examples/snapshot-controller.yaml";

fn load() -> SnapshotController {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a SnapshotController")
}

#[test]
fn example_is_a_single_snapshot_controller_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "SnapshotController");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    snapshot_controller_reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_leaves_group_snapshots_disabled_by_default() {
    let spec = load().spec;

    assert!(!spec.group_snapshots_enabled);

    let values = build_values(&spec);

    assert_eq!(values["installCRDs"], true);
    assert_eq!(values["webhook"]["enabled"], false);
}
