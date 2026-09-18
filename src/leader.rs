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
