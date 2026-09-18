use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::api::PostParams;
use kube::{Api, Client};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::time::Instant as TokioInstant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseState {
    pub holder_identity: Option<String>,
    pub renew_time_unix_seconds: Option<i64>,
    pub resource_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAction {
    Create,
    Acquire { resource_version: String },
    Renew { resource_version: String },
    Wait,
}

pub fn decide_lease_action(
    lease: Option<&LeaseState>,
    identity: &str,
    now_unix_seconds: i64,
    lease_duration_seconds: i64,
) -> LeaseAction {
    let Some(state) = lease else {
        return LeaseAction::Create;
    };

    let expired = state
        .renew_time_unix_seconds
        .map(|renew| now_unix_seconds > renew + lease_duration_seconds)
        .unwrap_or(true);

    match &state.holder_identity {
        Some(holder) if holder == identity => LeaseAction::Renew {
            resource_version: state.resource_version.clone(),
        },
        Some(_) if !expired => LeaseAction::Wait,
        _ => LeaseAction::Acquire {
            resource_version: state.resource_version.clone(),
        },
    }
}

pub const LEASE_DURATION_SECONDS: i64 = 15;
pub const RENEW_DEADLINE: StdDuration = StdDuration::from_secs(10);
pub const RETRY_PERIOD: StdDuration = StdDuration::from_secs(2);

fn lease_state_from(lease: &Lease) -> LeaseState {
    let spec = lease.spec.as_ref();
    LeaseState {
        holder_identity: spec.and_then(|s| s.holder_identity.clone()),
        renew_time_unix_seconds: spec
            .and_then(|s| s.renew_time.as_ref())
            .map(|t| t.0.as_second()),
        resource_version: lease.metadata.resource_version.clone().unwrap_or_default(),
    }
}

fn build_lease(
    name: &str,
    identity: &str,
    lease_duration_seconds: i64,
    now: Timestamp,
    lease_transitions: i32,
    acquire_time: Option<MicroTime>,
    resource_version: Option<String>,
) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            resource_version,
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(identity.to_string()),
            lease_duration_seconds: Some(lease_duration_seconds as i32),
            acquire_time: Some(acquire_time.unwrap_or(MicroTime(now))),
            renew_time: Some(MicroTime(now)),
            lease_transitions: Some(lease_transitions),
            preferred_holder: None,
            strategy: None,
        }),
    }
}

pub async fn run(
    client: Client,
    namespace: String,
    lease_name: String,
    identity: String,
    is_leader: Arc<AtomicBool>,
) {
    let api: Api<Lease> = Api::namespaced(client, &namespace);

    loop {
        // Wall-clock-bound the fetch. `kube`'s client defaults to no read
        // timeout and retries internally, so a wedged apiserver connection can
        // otherwise park this loop for minutes while `is_leader` stays latched
        // at whatever it last was — a stuck "leader" long after the lease
        // expired and a standby took over. Dropping the future (which is what
        // an elapsed `timeout` does) cancels the in-flight hyper request rather
        // than leaking it.
        let existing = match tokio::time::timeout(RENEW_DEADLINE, api.get_opt(&lease_name)).await {
            Ok(Ok(existing)) => existing,
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "failed to fetch lease, retrying");
                is_leader.store(false, Ordering::Relaxed);
                tokio::time::sleep(RETRY_PERIOD).await;
                continue;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_seconds = RENEW_DEADLINE.as_secs(),
                    "timed out fetching lease, retrying"
                );
                is_leader.store(false, Ordering::Relaxed);
                tokio::time::sleep(RETRY_PERIOD).await;
                continue;
            }
        };

        let now = Timestamp::now();
        let state = existing.as_ref().map(lease_state_from);
        let action = decide_lease_action(state.as_ref(), &identity, now.as_second(), LEASE_DURATION_SECONDS);

        match action {
            LeaseAction::Create => {
                let lease = build_lease(&lease_name, &identity, LEASE_DURATION_SECONDS, now, 0, None, None);
                match tokio::time::timeout(RENEW_DEADLINE, api.create(&PostParams::default(), &lease)).await {
                    Ok(Ok(_)) => {
                        tracing::info!(identity = %identity, "acquired leadership (created lease)");
                        is_leader.store(true, Ordering::Relaxed);
                    }
                    Ok(Err(err)) => {
                        tracing::debug!(error = %err, "failed to create lease, likely lost race");
                        is_leader.store(false, Ordering::Relaxed);
                    }
                    Err(_elapsed) => {
                        // Not a lost race — a wedged request. Bounded so this
                        // loop cannot silently stall forever unable to acquire.
                        tracing::warn!(
                            timeout_seconds = RENEW_DEADLINE.as_secs(),
                            "timed out creating lease, retrying"
                        );
                        is_leader.store(false, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(RETRY_PERIOD).await;
            }
            LeaseAction::Acquire { resource_version } => {
                let transitions = existing
                    .as_ref()
                    .and_then(|lease| lease.spec.as_ref())
                    .and_then(|spec| spec.lease_transitions)
                    .unwrap_or(0)
                    + 1;
                let lease = build_lease(
                    &lease_name,
                    &identity,
                    LEASE_DURATION_SECONDS,
                    now,
                    transitions,
                    None,
                    Some(resource_version),
                );
                match tokio::time::timeout(
                    RENEW_DEADLINE,
                    api.replace(&lease_name, &PostParams::default(), &lease),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        tracing::info!(identity = %identity, "acquired leadership");
                        is_leader.store(true, Ordering::Relaxed);
                    }
                    Ok(Err(err)) => {
                        tracing::debug!(error = %err, "failed to acquire lease, likely lost race");
                        is_leader.store(false, Ordering::Relaxed);
                    }
                    Err(_elapsed) => {
                        tracing::warn!(
                            timeout_seconds = RENEW_DEADLINE.as_secs(),
                            "timed out acquiring lease, retrying"
                        );
                        is_leader.store(false, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(RETRY_PERIOD).await;
            }
            LeaseAction::Renew { resource_version } => {
                hold_and_renew(&api, &lease_name, &identity, existing, resource_version, &is_leader).await;
            }
            LeaseAction::Wait => {
                is_leader.store(false, Ordering::Relaxed);
                tokio::time::sleep(RETRY_PERIOD).await;
            }
        }
    }
}

async fn hold_and_renew(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    mut existing: Option<Lease>,
    mut resource_version: String,
    is_leader: &AtomicBool,
) {
    // Every networked call below is bounded by this single absolute instant via
    // `timeout_at`, not by a fresh `RENEW_DEADLINE` each time. That is the whole
    // point: whatever is left of the budget when an attempt starts is exactly
    // how long that attempt gets, so the total wall-clock time this function can
    // spend across all of its retries and API calls combined stays capped at
    // RENEW_DEADLINE. A flat `timeout(RENEW_DEADLINE, ...)` per attempt would
    // let one hung call consume the entire budget and then some, which is how a
    // replica ends up still believing `is_leader == true` well after the lease's
    // LEASE_DURATION_SECONDS expiry let a standby take over — split brain.
    let deadline = TokioInstant::now() + RENEW_DEADLINE;
    loop {
        let now = Timestamp::now();
        let transitions = existing
            .as_ref()
            .and_then(|lease| lease.spec.as_ref())
            .and_then(|spec| spec.lease_transitions)
            .unwrap_or(0);
        let acquire_time = existing
            .as_ref()
            .and_then(|lease| lease.spec.as_ref())
            .and_then(|spec| spec.acquire_time.clone());
        let lease = build_lease(
            lease_name,
            identity,
            LEASE_DURATION_SECONDS,
            now,
            transitions,
            acquire_time,
            Some(resource_version.clone()),
        );

        match tokio::time::timeout_at(deadline, api.replace(lease_name, &PostParams::default(), &lease)).await {
            Ok(Ok(_)) => {
                is_leader.store(true, Ordering::Relaxed);
                // Pacing before the next renewal attempt, not part of the
                // renewal budget: the lease was just refreshed, so sleeping
                // here cannot strand a stale leader.
                tokio::time::sleep(RETRY_PERIOD).await;
                return;
            }
            // Both a real API error and an elapsed timeout mean "this attempt
            // failed"; they only differ in what gets logged, so they converge on
            // the shared deadline-check/back-off/refetch path below.
            Ok(Err(err)) => tracing::warn!(error = %err, "lease renewal failed"),
            Err(_elapsed) => tracing::warn!(
                identity = %identity,
                "lease renewal timed out against the renew deadline"
            ),
        }

        if TokioInstant::now() >= deadline {
            tracing::warn!(identity = %identity, "lost leadership after failing to renew within deadline");
            is_leader.store(false, Ordering::Relaxed);
            return;
        }
        // Clamp the back-off to the deadline too, so the retry pause cannot
        // itself push this function past its budget.
        tokio::time::sleep_until(std::cmp::min(TokioInstant::now() + RETRY_PERIOD, deadline)).await;
        match tokio::time::timeout_at(deadline, api.get_opt(lease_name)).await {
            Ok(Ok(Some(refreshed))) => {
                // Crucially, re-check the holder. If another replica acquired the
                // lease while we were failing to renew, we have genuinely lost it.
                // Looping round to `replace` with ourselves as holder would steal
                // it back from a replica that has already set its own
                // `is_leader = true` — two active leaders at once, which is the
                // exact split brain this module exists to prevent.
                let still_ours = refreshed
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.holder_identity.as_deref())
                    == Some(identity);
                if !still_ours {
                    tracing::warn!(
                        identity = %identity,
                        "lost leadership: lease was acquired by another replica while renewal was failing"
                    );
                    is_leader.store(false, Ordering::Relaxed);
                    return;
                }
                resource_version = refreshed.metadata.resource_version.clone().unwrap_or_default();
                existing = Some(refreshed);
            }
            Ok(Ok(None)) => {
                is_leader.store(false, Ordering::Relaxed);
                return;
            }
            Ok(Err(_)) | Err(_) => {
                // Keep retrying with the same resource_version; it will
                // conflict-fail again and this loop re-checks the
                // deadline on the next iteration.
            }
        }
    }
}

pub async fn release(client: Client, namespace: String, lease_name: String, identity: String) {
    let api: Api<Lease> = Api::namespaced(client, &namespace);
    // Both calls here are bounded too: this runs on the SIGTERM path with the
    // process waiting on it, so an unbounded request would stall shutdown until
    // the kubelet's grace period expired and SIGKILLed us — delaying the very
    // handoff this function exists to speed up.
    let existing = match tokio::time::timeout(RENEW_DEADLINE, api.get_opt(&lease_name)).await {
        Ok(Ok(Some(existing))) => existing,
        Err(_elapsed) => {
            tracing::warn!("timed out fetching lease for release on shutdown");
            return;
        }
        _ => return,
    };

    let is_holder = existing
        .spec
        .as_ref()
        .and_then(|spec| spec.holder_identity.as_deref())
        == Some(identity.as_str());
    if !is_holder {
        return;
    }

    let expired_time = match Timestamp::from_second(Timestamp::now().as_second() - LEASE_DURATION_SECONDS - 1) {
        Ok(time) => time,
        Err(_) => return,
    };
    let resource_version = existing.metadata.resource_version.clone();
    let lease = Lease {
        metadata: ObjectMeta {
            name: Some(lease_name.clone()),
            resource_version,
            ..Default::default()
        },
        spec: existing.spec.map(|mut spec| {
            spec.renew_time = Some(MicroTime(expired_time));
            spec
        }),
    };

    match tokio::time::timeout(
        RENEW_DEADLINE,
        api.replace(&lease_name, &PostParams::default(), &lease),
    )
    .await
    {
        Ok(Ok(_)) => tracing::info!(identity = %identity, "released lease on shutdown"),
        Ok(Err(err)) => tracing::warn!(error = %err, "failed to release lease on shutdown"),
        Err(_elapsed) => tracing::warn!("timed out releasing lease on shutdown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(holder: Option<&str>, renew_time: Option<i64>) -> LeaseState {
        LeaseState {
            holder_identity: holder.map(str::to_string),
            renew_time_unix_seconds: renew_time,
            resource_version: "1".to_string(),
        }
    }

    #[test]
    fn no_lease_returns_create() {
        assert_eq!(decide_lease_action(None, "me", 100, 15), LeaseAction::Create);
    }

    #[test]
    fn held_by_self_returns_renew() {
        let lease = state(Some("me"), Some(100));
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 105, 15),
            LeaseAction::Renew {
                resource_version: "1".to_string()
            }
        );
    }

    #[test]
    fn held_by_other_not_expired_returns_wait() {
        let lease = state(Some("other"), Some(100));
        assert_eq!(decide_lease_action(Some(&lease), "me", 110, 15), LeaseAction::Wait);
    }

    #[test]
    fn held_by_other_expired_returns_acquire() {
        let lease = state(Some("other"), Some(100));
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 200, 15),
            LeaseAction::Acquire {
                resource_version: "1".to_string()
            }
        );
    }

    #[test]
    fn held_by_self_but_already_expired_still_returns_renew() {
        // Holder-is-self takes priority over expiry: `hold_and_renew` is the
        // right next step even for a lease we have let go stale. This is also
        // why that function must re-check the holder after a failed renewal —
        // it can legitimately be entered on an expired lease that another
        // replica is about to (or already has) taken over.
        let lease = state(Some("me"), Some(100));
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 500, 15),
            LeaseAction::Renew {
                resource_version: "1".to_string()
            }
        );
    }

    #[test]
    fn held_by_other_with_no_renew_time_returns_acquire() {
        let lease = state(Some("other"), None);
        assert_eq!(
            decide_lease_action(Some(&lease), "me", 100, 15),
            LeaseAction::Acquire {
                resource_version: "1".to_string()
            }
        );
    }
}
