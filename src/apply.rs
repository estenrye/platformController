use crate::crd::AppliedResourceRef;

pub fn resources_to_prune(
    previous: &[AppliedResourceRef],
    current: &[AppliedResourceRef],
) -> Vec<AppliedResourceRef> {
    previous
        .iter()
        .filter(|candidate| !current.contains(candidate))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::AppliedResourceRef;

    fn resource(kind: &str, name: &str) -> AppliedResourceRef {
        AppliedResourceRef {
            api_version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn returns_resources_present_before_but_missing_now() {
        let previous = vec![resource("ConfigMap", "a"), resource("ConfigMap", "b")];
        let current = vec![resource("ConfigMap", "b"), resource("ConfigMap", "c")];

        let pruned = resources_to_prune(&previous, &current);

        assert_eq!(pruned, vec![resource("ConfigMap", "a")]);
    }

    #[test]
    fn returns_empty_when_nothing_removed() {
        let previous = vec![resource("ConfigMap", "a")];
        let current = vec![resource("ConfigMap", "a"), resource("ConfigMap", "b")];

        assert!(resources_to_prune(&previous, &current).is_empty());
    }
}
