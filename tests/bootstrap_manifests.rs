use platform_controller::manifests::{parse_manifests, sort_manifests};

#[test]
fn bootstrap_yaml_parses_into_expected_kinds_in_apply_order() {
    let content = std::fs::read_to_string("deploy/bootstrap.yaml").expect("bootstrap.yaml should exist");
    let mut objects = parse_manifests(&content).expect("bootstrap.yaml should be valid YAML documents");
    sort_manifests(&mut objects);

    let kinds: Vec<String> = objects
        .iter()
        .map(|o| o.types.as_ref().unwrap().kind.clone())
        .collect();

    assert_eq!(
        kinds,
        vec![
            "Namespace",
            "ServiceAccount",
            "ClusterRoleBinding",
            "Deployment",
            "PodDisruptionBudget",
        ]
    );
}

#[test]
fn example_cni_installation_yaml_defines_a_single_cni_installation() {
    let content = std::fs::read_to_string("examples/cni-installation.yaml")
        .expect("examples/cni-installation.yaml should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CniInstallation");
    assert_eq!(objects[0].metadata.name.as_deref(), Some("default"));
}

#[test]
fn crd_yaml_defines_the_cniinstallation_resource() {
    let content = std::fs::read_to_string("deploy/crd.yaml")
        .expect("deploy/crd.yaml should exist; run `cargo run --bin crdgen > deploy/crd.yaml`");
    let objects = parse_manifests(&content).expect("crd.yaml should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "CustomResourceDefinition");
    assert_eq!(
        objects[0].metadata.name.as_deref(),
        Some("cniinstallations.platform.rye.ninja")
    );
}

#[test]
fn crd_yaml_matches_the_generated_crd() {
    use kube::CustomResourceExt;

    let generated = serde_yaml::to_string(&platform_controller::crd::CniInstallation::crd())
        .expect("CRD should serialize to YAML");
    let on_disk = std::fs::read_to_string("deploy/crd.yaml").expect("deploy/crd.yaml should exist");

    assert_eq!(
        on_disk, generated,
        "deploy/crd.yaml is stale; run `cargo run -q --bin crdgen > deploy/crd.yaml`"
    );
}
