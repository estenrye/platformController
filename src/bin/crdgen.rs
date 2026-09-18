use kube::CustomResourceExt;

fn main() {
    let crd = platform_controller::crd::CniInstallation::crd();
    print!("{}", serde_yaml::to_string(&crd).expect("CRD should serialize to YAML"));
}
