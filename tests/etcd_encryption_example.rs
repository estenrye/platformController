use platform_controller::etcd_encryption::EtcdEncryption;
use platform_controller::etcd_encryption_reconciler;
use platform_controller::manifests::parse_manifests;

const EXAMPLE: &str = "examples/etcd-encryption.yaml";

fn load() -> EtcdEncryption {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as an EtcdEncryption")
}

#[test]
fn example_is_a_single_etcd_encryption_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "EtcdEncryption");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let installation = load();

    assert_eq!(installation.metadata.name.as_deref(), Some("default"));
    etcd_encryption_reconciler::validate("default", &installation.spec).expect("the example must be valid");
}

#[test]
fn example_starts_with_every_acknowledgement_false() {
    let acks = load().spec.acknowledgements;

    assert!(!acks.kms_config_applied && !acks.plaintext_removed && !acks.kms_reverted && !acks.kms_removed);
}

#[test]
fn example_builds_a_plugin_plan() {
    let plan = platform_controller::kms_provider::kms_plan(&load().spec).expect("plan builds");

    assert_eq!(plan.daemonset_name, "barbican-kms");
}
