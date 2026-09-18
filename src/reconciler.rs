use crate::crd::{
    AppliedResourceRef, CniInstallation, CniInstallationSpec, CniInstallationStatus, CniProvider,
    Condition, Phase, PlatformKind,
};
use kube::runtime::controller::Action;
use kube::{Client, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

pub struct Context {
    pub client: Client,
}

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("unsupported platformKind {0:?}, only TalosLinux is supported")]
    UnsupportedPlatform(PlatformKind),
    #[error("unsupported provider {0:?}, only Calico is supported")]
    UnsupportedProvider(CniProvider),
}

pub fn validate(spec: &CniInstallationSpec) -> Result<(), ValidationError> {
    if spec.platform_kind != PlatformKind::TalosLinux {
        return Err(ValidationError::UnsupportedPlatform(spec.platform_kind.clone()));
    }
    if spec.provider != CniProvider::Calico {
        return Err(ValidationError::UnsupportedProvider(spec.provider.clone()));
    }
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Helm(#[from] crate::helm::HelmError),
    #[error(transparent)]
    Manifest(#[from] crate::manifests::ManifestError),
    #[error(transparent)]
    Apply(#[from] crate::apply::ApplyError),
    #[error("failed to update status: {0}")]
    Status(#[source] kube::Error),
}

pub async fn reconcile(obj: Arc<CniInstallation>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let api: kube::Api<CniInstallation> = kube::Api::all(ctx.client.clone());
    let chart_version = obj.spec.calico.chart_version.clone();

    let previous = obj
        .status
        .as_ref()
        .map(|status| status.applied_resources.clone())
        .unwrap_or_default();

    if let Err(err) = validate(&obj.spec) {
        update_status(
            &api,
            &name,
            Phase::Failed,
            obj.metadata.generation,
            &chart_version,
            &previous,
            "Unsupported",
            &err.to_string(),
        )
        .await?;
        return Err(ReconcileError::Validation(err));
    }

    let rendered = crate::helm::render(&obj.spec.calico).await?;
    let mut objects = crate::manifests::parse_manifests(&rendered)?;
    crate::manifests::sort_manifests(&mut objects);

    let mut applied = Vec::new();
    for object in &objects {
        let reference = crate::apply::apply_object(&ctx.client, object, "platform-controller").await?;
        applied.push(reference);
    }

    for stale in crate::apply::resources_to_prune(&previous, &applied) {
        crate::apply::delete_object(&ctx.client, &stale).await?;
    }

    update_status(
        &api,
        &name,
        Phase::Ready,
        obj.metadata.generation,
        &chart_version,
        &applied,
        "Applied",
        "manifests applied successfully",
    )
    .await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy(_obj: Arc<CniInstallation>, _err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    Action::requeue(Duration::from_secs(30))
}

async fn update_status(
    api: &kube::Api<CniInstallation>,
    name: &str,
    phase: Phase,
    generation: Option<i64>,
    chart_version: &str,
    applied_resources: &[AppliedResourceRef],
    reason: &str,
    message: &str,
) -> Result<(), ReconcileError> {
    let condition = Condition {
        type_: "Applied".to_string(),
        status: if matches!(phase, Phase::Ready) { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
        observed_generation: generation,
    };

    let status = CniInstallationStatus {
        phase,
        observed_generation: generation.unwrap_or(0),
        chart_version: chart_version.to_string(),
        applied_resources: applied_resources.to_vec(),
        conditions: vec![condition],
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(patch))
        .await
        .map_err(ReconcileError::Status)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{CalicoSpec, CniInstallationSpec, CniProvider, PlatformKind};

    fn spec_with(platform_kind: PlatformKind, provider: CniProvider) -> CniInstallationSpec {
        CniInstallationSpec {
            platform_kind,
            provider,
            calico: CalicoSpec {
                chart_version: "v3.29.1".to_string(),
                bgp_enabled: false,
                api_server_enabled: false,
                ip_pools: vec![],
                node_address_autodetection_v6_cidrs: vec![],
            },
        }
    }

    #[test]
    fn accepts_talos_linux_calico() {
        let spec = spec_with(PlatformKind::TalosLinux, CniProvider::Calico);
        assert!(validate(&spec).is_ok());
    }
}
