use crate::crd::AppliedResourceRef;

/// What a reconcile is about to apply, recorded as soon as it is known so a
/// failure can still persist a ledger covering everything that may exist.
#[derive(Debug, Default)]
pub struct ReconcileProgress {
    pub desired: Option<Vec<AppliedResourceRef>>,
}

/// `desired` in apply order, followed by any `previous` entries not in
/// `desired`. Duplicates are removed; the first occurrence wins.
///
/// Order matters: CNI cleanup deletes in reverse ledger order (and moves the
/// operator's `Installation` last), so apply order must be preserved. Stale
/// entries go last so cleanup, which reverses, removes them first.
pub fn merge_ledger(
    previous: &[AppliedResourceRef],
    desired: &[AppliedResourceRef],
) -> Vec<AppliedResourceRef> {
    let mut merged: Vec<AppliedResourceRef> = Vec::with_capacity(previous.len().max(desired.len()));
    for reference in desired.iter().chain(previous.iter()) {
        if !merged.contains(reference) {
            merged.push(reference.clone());
        }
    }
    merged
}

/// True when `desired` holds an entry missing from `previous`, i.e. the
/// reconcile is about to create something the saved ledger does not know about.
pub fn ledger_needs_checkpoint(
    previous: &[AppliedResourceRef],
    desired: &[AppliedResourceRef],
) -> bool {
    desired.iter().any(|reference| !previous.contains(reference))
}

/// The ledger to persist before applying, or `None` in steady state, so a
/// periodic resync writes nothing extra.
pub fn checkpoint_ledger(
    previous: &[AppliedResourceRef],
    desired: &[AppliedResourceRef],
) -> Option<Vec<AppliedResourceRef>> {
    ledger_needs_checkpoint(previous, desired).then(|| merge_ledger(previous, desired))
}

/// The ledger to persist when a reconcile fails: everything that may exist.
pub fn failure_ledger(
    previous: &[AppliedResourceRef],
    desired: Option<&[AppliedResourceRef]>,
) -> Vec<AppliedResourceRef> {
    match desired {
        Some(desired) => merge_ledger(previous, desired),
        None => previous.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(kind: &str, name: &str) -> AppliedResourceRef {
        AppliedResourceRef {
            api_version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn merge_keeps_desired_in_apply_order_then_stale_previous_entries() {
        let previous = vec![resource("Service", "old"), resource("Namespace", "ns")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert_eq!(
            merge_ledger(&previous, &desired),
            vec![
                resource("Namespace", "ns"),
                resource("DaemonSet", "ds"),
                resource("Service", "old"),
            ]
        );
    }

    #[test]
    fn merge_removes_duplicates_first_occurrence_wins() {
        let desired = vec![
            resource("Namespace", "ns"),
            resource("DaemonSet", "ds"),
            resource("Namespace", "ns"),
        ];

        assert_eq!(
            merge_ledger(&[], &desired),
            vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")]
        );
    }

    #[test]
    fn merge_of_empty_inputs_is_empty() {
        assert!(merge_ledger(&[], &[]).is_empty());
    }

    #[test]
    fn a_first_install_needs_a_checkpoint() {
        let desired = vec![resource("Namespace", "ns")];

        assert!(ledger_needs_checkpoint(&[], &desired));
    }

    #[test]
    fn an_identical_ledger_needs_no_checkpoint() {
        let ledger = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert!(!ledger_needs_checkpoint(&ledger, &ledger));
    }

    #[test]
    fn a_reordered_ledger_needs_no_checkpoint() {
        let previous = vec![resource("DaemonSet", "ds"), resource("Namespace", "ns")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert!(!ledger_needs_checkpoint(&previous, &desired));
    }

    #[test]
    fn a_new_object_needs_a_checkpoint() {
        let previous = vec![resource("Namespace", "ns")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert!(ledger_needs_checkpoint(&previous, &desired));
    }

    #[test]
    fn dropping_an_object_alone_needs_no_checkpoint() {
        // Pruning is handled after applying, against the previous ledger; a
        // checkpoint only protects objects that are about to be created.
        let previous = vec![resource("Namespace", "ns"), resource("Service", "old")];
        let desired = vec![resource("Namespace", "ns")];

        assert!(!ledger_needs_checkpoint(&previous, &desired));
    }

    #[test]
    fn checkpoint_is_none_in_steady_state_and_the_merged_ledger_otherwise() {
        let previous = vec![resource("Namespace", "ns")];
        let grown = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert_eq!(checkpoint_ledger(&previous, &previous), None);
        assert_eq!(
            checkpoint_ledger(&previous, &grown),
            Some(vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")])
        );
    }

    #[test]
    fn a_failure_before_desired_is_known_keeps_the_previous_ledger() {
        let previous = vec![resource("Namespace", "ns")];

        assert_eq!(failure_ledger(&previous, None), previous);
    }

    #[test]
    fn a_failure_after_desired_is_known_covers_everything_that_may_exist() {
        let previous = vec![resource("Service", "old")];
        let desired = vec![resource("Namespace", "ns"), resource("DaemonSet", "ds")];

        assert_eq!(
            failure_ledger(&previous, Some(&desired)),
            vec![
                resource("Namespace", "ns"),
                resource("DaemonSet", "ds"),
                resource("Service", "old"),
            ]
        );
    }

    #[test]
    fn progress_starts_with_no_desired_list() {
        assert_eq!(ReconcileProgress::default().desired, None);
    }
}
