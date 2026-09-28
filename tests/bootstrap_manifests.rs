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
fn controller_deployment_tolerates_the_uninitialized_cloud_provider_taint() {
    // With kubelets on --cloud-provider=external every node starts tainted
    // node.cloudprovider.kubernetes.io/uninitialized until the CCM initializes
    // it. This controller installs the CCM, so it has to schedule first.
    let content = std::fs::read_to_string("deploy/bootstrap.yaml").expect("bootstrap.yaml should exist");
    let objects = parse_manifests(&content).expect("bootstrap.yaml should be valid YAML documents");
    let deployment = objects
        .iter()
        .find(|o| o.types.as_ref().unwrap().kind == "Deployment")
        .expect("bootstrap.yaml has the controller Deployment");

    let tolerations = deployment
        .data
        .pointer("/spec/template/spec/tolerations")
        .and_then(|value| value.as_array())
        .expect("the Deployment has tolerations");

    assert!(
        tolerations.iter().any(|toleration| {
            toleration["key"] == "node.cloudprovider.kubernetes.io/uninitialized"
                && toleration["operator"] == "Exists"
                && toleration["effect"] == "NoSchedule"
        }),
        "{tolerations:?}"
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
fn crd_yaml_defines_all_platform_resources() {
    let content = std::fs::read_to_string("deploy/crd.yaml")
        .expect("deploy/crd.yaml should exist; run `cargo run -q --bin crdgen > deploy/crd.yaml`");
    let objects = parse_manifests(&content).expect("crd.yaml should be valid YAML");

    let names: Vec<String> = objects
        .iter()
        .map(|o| {
            assert_eq!(o.types.as_ref().unwrap().kind, "CustomResourceDefinition");
            o.metadata.name.clone().unwrap()
        })
        .collect();

    assert_eq!(
        names,
        vec![
            "cniinstallations.platform.rye.ninja",
            "pullthroughcaches.platform.rye.ninja",
            "cloudcontrollermanagers.platform.rye.ninja",
            "csidrivers.platform.rye.ninja",
        ]
    );
}

#[test]
fn crd_yaml_matches_the_generated_crds() {
    let generated = platform_controller::crds::generated_yaml();
    let on_disk = std::fs::read_to_string("deploy/crd.yaml").expect("deploy/crd.yaml should exist");

    assert_eq!(
        on_disk, generated,
        "deploy/crd.yaml is stale; run `cargo run -q --bin crdgen > deploy/crd.yaml`"
    );
}
