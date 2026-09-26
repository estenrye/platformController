use platform_controller::manifests::parse_manifests;
use platform_controller::pull_through_cache::{build_values, PullThroughCache};
use platform_controller::cache_reconciler;

const EXAMPLE: &str = "examples/pull-through-cache.yaml";

fn load() -> PullThroughCache {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    serde_yaml::from_str(&content).expect("the example should deserialize as a PullThroughCache")
}

#[test]
fn example_is_a_single_pull_through_cache_document() {
    let content = std::fs::read_to_string(EXAMPLE).expect("the example should exist");
    let objects = parse_manifests(&content).expect("example should be valid YAML");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].types.as_ref().unwrap().kind, "PullThroughCache");
}

#[test]
fn example_is_the_singleton_and_passes_validation() {
    let cache = load();

    assert_eq!(cache.metadata.name.as_deref(), Some("default"));
    cache_reconciler::validate("default", &cache.spec).expect("the example must be valid");
}

#[test]
fn example_names_its_registries_explicitly() {
    // Omitting `registries` mirrors every registry, private ones included, so the
    // starting point must make the choice visible.
    let spegel = load().spec.spegel;

    let registries = spegel.registries.expect("the example sets registries explicitly");
    assert!(registries.contains(&"docker.io".to_string()));
}

#[test]
fn example_chart_version_has_no_v_prefix() {
    // The OCI chart tag is `0.7.4`; `v0.7.4` is only the GitHub release tag and
    // does not exist on ghcr.io ("not found").
    let version = load().spec.spegel.chart_version;

    assert!(!version.starts_with('v'), "{version}");
}

#[test]
fn example_values_carry_the_talos_path_and_the_registries() {
    let spegel = load().spec.spegel;
    let values = build_values(&spegel);

    assert_eq!(
        values["spegel"]["containerdRegistryConfigPath"],
        "/etc/cri/conf.d/hosts"
    );
    assert_eq!(
        values["spegel"]["mirroredRegistries"],
        serde_json::json!(spegel.registries.unwrap())
    );
}
