use std::collections::BTreeMap;

/// Top-level storage prefixes begin with this. Inner sub-prefixes (such as the
/// `key2:` seen under secretbox) describe the same operation and are ignored so
/// reads are not double-counted.
pub const TOP_LEVEL_PREFIX: &str = "k8s:enc:";

const FAMILY: &str = "apiserver_storage_transformation_operations_total";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    FromStorage,
    ToStorage,
}

/// Secrets transformation counters by `(direction, top-level prefix)`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Transformations(pub BTreeMap<(Direction, String), i64>);

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Delta {
    pub from_storage: BTreeMap<String, i64>,
    pub to_storage: BTreeMap<String, i64>,
}

/// Parses `key="value",key2="value2"` (the inside of a metric's braces).
fn parse_labels(labels: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = labels.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let Some(after) = after.strip_prefix('"') else { break };
        let Some(end) = after.find('"') else { break };
        out.insert(key, after[..end].to_string());
        rest = after[end + 1..].trim_start().trim_start_matches(',');
    }
    out
}

/// The Secrets `apiserver_storage_transformation_operations_total` counters
/// with `status="OK"`, keeping only top-level prefixes (`k8s:enc:...`) and the
/// empty prefix. Duplicate series are summed.
pub fn parse_secret_transformations(metrics: &str) -> Transformations {
    let mut out: BTreeMap<(Direction, String), i64> = BTreeMap::new();
    for line in metrics.lines() {
        let Some(rest) = line.strip_prefix(FAMILY) else { continue };
        let Some(rest) = rest.strip_prefix('{') else { continue };
        let Some(close) = rest.rfind('}') else { continue };
        let labels = parse_labels(&rest[..close]);
        if labels.get("resource").map(String::as_str) != Some("secrets")
            || labels.get("status").map(String::as_str) != Some("OK")
        {
            continue;
        }
        let direction = match labels.get("transformation_type").map(String::as_str) {
            Some("from_storage") => Direction::FromStorage,
            Some("to_storage") => Direction::ToStorage,
            _ => continue,
        };
        let prefix = labels.get("transformer_prefix").cloned().unwrap_or_default();
        if !(prefix.is_empty() || prefix.starts_with(TOP_LEVEL_PREFIX)) {
            continue;
        }
        let Some(value_str) = rest[close + 1..].split_whitespace().next() else { continue };
        let Ok(value) = value_str.parse::<f64>() else { continue };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        let rounded = value.round() as i64;
        out.entry((direction, prefix))
            .and_modify(|v| *v = v.saturating_add(rounded))
            .or_insert(rounded);
    }
    Transformations(out)
}

const PROCESS_START: &str = "process_start_time_seconds";

/// The apiserver's `process_start_time_seconds` gauge (the first token after
/// the name), or `None` if it is absent or not a finite number. Two snapshots
/// with different values were taken across a restart.
pub fn parse_process_start_time(metrics: &str) -> Option<f64> {
    metrics.lines().find_map(|line| {
        let rest = line.strip_prefix(PROCESS_START)?;
        let rest = match rest.strip_prefix('{') {
            Some(labels) => &labels[labels.rfind('}')? + 1..],
            None => rest,
        };
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        rest.split_whitespace().next()?.parse::<f64>().ok().filter(|v| v.is_finite())
    })
}

/// The increase between two snapshots, positive entries only. `None` if any
/// series decreased or vanished: an apiserver restart reset its counters, so
/// the difference means nothing and must never be read as "no reads".
pub fn delta(before: &Transformations, after: &Transformations) -> Option<Delta> {
    if before.0.iter().any(|(k, b)| after.0.get(k).map_or(*b > 0, |a| a < b)) {
        return None;
    }
    let mut out = Delta::default();
    for ((direction, prefix), a) in &after.0 {
        let before_val = before.0.get(&(*direction, prefix.clone())).copied().unwrap_or(0);
        let d = a.saturating_sub(before_val);
        if d > 0 {
            match direction {
                Direction::FromStorage => out.from_storage.insert(prefix.clone(), d),
                Direction::ToStorage => out.to_storage.insert(prefix.clone(), d),
            };
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Taken from a real Talos cluster (2026-10-01), plus a HELP/TYPE header.
    const LIVE: &str = r#"# HELP apiserver_storage_transformation_operations_total [ALPHA] Total number of transformations.
# TYPE apiserver_storage_transformation_operations_total counter
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 284
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:secretbox:v1:"} 86
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="key2:"} 86
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="to_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 1
apiserver_request_total{code="200"} 5
"#;

    fn key(direction: Direction, prefix: &str) -> (Direction, String) {
        (direction, prefix.to_string())
    }

    #[test]
    fn parses_the_live_sample_and_ignores_the_inner_key_prefix() {
        let parsed = parse_secret_transformations(LIVE).0;

        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&key(Direction::FromStorage, "k8s:enc:kms:v2:barbican:")], 284);
        assert_eq!(parsed[&key(Direction::FromStorage, "k8s:enc:secretbox:v1:")], 86);
        assert_eq!(parsed[&key(Direction::ToStorage, "k8s:enc:kms:v2:barbican:")], 1);
        assert!(!parsed.contains_key(&key(Direction::FromStorage, "key2:")));
    }

    #[test]
    fn label_order_does_not_matter() {
        let line = r#"apiserver_storage_transformation_operations_total{transformer_prefix="k8s:enc:aescbc:v1:k:",transformation_type="from_storage",status="OK",resource="secrets"} 7"#;

        assert_eq!(
            parse_secret_transformations(line).0[&key(Direction::FromStorage, "k8s:enc:aescbc:v1:k:")],
            7
        );
    }

    #[test]
    fn other_resources_and_non_ok_statuses_are_ignored() {
        let text = r#"apiserver_storage_transformation_operations_total{resource="configmaps",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 9
apiserver_storage_transformation_operations_total{resource="secrets",status="Error",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} 4"#;

        assert!(parse_secret_transformations(text).0.is_empty());
    }

    #[test]
    fn an_empty_prefix_is_kept_as_the_identity_reading() {
        let line = r#"apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix=""} 12"#;

        assert_eq!(parse_secret_transformations(line).0[&key(Direction::FromStorage, "")], 12);
    }

    #[test]
    fn an_empty_body_parses_to_nothing() {
        assert_eq!(parse_secret_transformations(""), Transformations::default());
    }

    #[test]
    fn delta_keeps_only_positive_differences_per_direction() {
        let before = parse_secret_transformations(LIVE);
        let after_text = LIVE
            .replace("transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 284", "transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 294")
            .replace("transformation_type=\"to_storage\",transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 1", "transformation_type=\"to_storage\",transformer_prefix=\"k8s:enc:kms:v2:barbican:\"} 2");
        let after = parse_secret_transformations(&after_text);

        let d = delta(&before, &after).expect("monotonic counters");

        assert_eq!(d.from_storage.len(), 1);
        assert_eq!(d.from_storage["k8s:enc:kms:v2:barbican:"], 10);
        assert_eq!(d.to_storage["k8s:enc:kms:v2:barbican:"], 1);
    }

    #[test]
    fn a_series_that_first_appears_counts_from_zero() {
        let before = Transformations::default();
        let after = parse_secret_transformations(LIVE);

        let d = delta(&before, &after).unwrap();

        assert_eq!(d.from_storage["k8s:enc:secretbox:v1:"], 86);
    }

    #[test]
    fn a_decreasing_counter_is_a_reset_not_a_delta() {
        // Review Focus 4: an apiserver restart mid-check.
        let before = parse_secret_transformations(LIVE);
        let after = parse_secret_transformations(&LIVE.replace("} 284", "} 3"));

        assert_eq!(delta(&before, &after), None);
    }

    #[test]
    fn a_vanished_series_is_a_reset_not_zero_reads() {
        // Review Focus 4: the counters disappeared (apiserver restarted).
        let before = parse_secret_transformations(LIVE);

        assert_eq!(delta(&before, &Transformations::default()), None);
    }

    #[test]
    fn trailing_timestamp_parses_to_the_right_value() {
        let line = r#"apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:aescbc:v1:"} 284 1700000000"#;

        assert_eq!(
            parse_secret_transformations(line).0[&key(Direction::FromStorage, "k8s:enc:aescbc:v1:")],
            284
        );
    }

    #[test]
    fn scientific_notation_parses_correctly() {
        let line = r#"apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:aescbc:v1:"} 1e+06"#;

        assert_eq!(
            parse_secret_transformations(line).0[&key(Direction::FromStorage, "k8s:enc:aescbc:v1:")],
            1000000
        );
    }

    #[test]
    fn invalid_values_are_ignored_without_panic() {
        let text = r#"apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:aescbc:v1:"} +Inf
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:secretbox:v1:"} NaN
apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:kms:v2:barbican:"} -42"#;

        let parsed = parse_secret_transformations(text).0;
        assert!(parsed.is_empty());
    }

    #[test]
    fn huge_duplicate_values_do_not_panic() {
        // Use 9.0e18 which is representable as f64 and near i64::MAX when converted
        let huge = "9.0e18";
        let line1 = format!(
            r#"apiserver_storage_transformation_operations_total{{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:aescbc:v1:"}} {}"#,
            huge
        );
        let line2 = format!(
            r#"apiserver_storage_transformation_operations_total{{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:aescbc:v1:"}} {}"#,
            huge
        );
        let text = format!("{}\n{}", line1, line2);

        // Should not panic; saturating_add prevents overflow
        let parsed = parse_secret_transformations(&text).0;
        assert!(parsed.contains_key(&key(Direction::FromStorage, "k8s:enc:aescbc:v1:")));
        // Value will be saturated at i64::MAX
        assert_eq!(
            parsed[&key(Direction::FromStorage, "k8s:enc:aescbc:v1:")],
            i64::MAX
        );
    }

    #[test]
    fn process_start_time_is_the_gauge_value() {
        let text = "# HELP process_start_time_seconds Start time of the process since unix epoch in seconds.\n\
                    # TYPE process_start_time_seconds gauge\n\
                    process_start_time_seconds 1.75926468212e+09\n";

        assert_eq!(parse_process_start_time(text), Some(1.75926468212e9));
        assert_eq!(parse_process_start_time(&format!("{LIVE}process_start_time_seconds 1700000000 1700000005\n")), Some(1.7e9));
    }

    #[test]
    fn process_start_time_is_absent_unless_the_gauge_has_a_finite_value() {
        assert_eq!(parse_process_start_time(LIVE), None);
        assert_eq!(parse_process_start_time(""), None);
        assert_eq!(parse_process_start_time("process_start_time_seconds NaN\n"), None);
        assert_eq!(parse_process_start_time("process_start_time_seconds +Inf\n"), None);
        assert_eq!(parse_process_start_time("process_start_time_seconds\n"), None);
        assert_eq!(parse_process_start_time("process_start_time_seconds_other 5\n"), None);
        assert_eq!(parse_process_start_time("# process_start_time_seconds 5\n"), None);
    }

    #[test]
    fn label_value_with_comma_and_brace_parses_correctly() {
        let line = r#"apiserver_storage_transformation_operations_total{resource="secrets",status="OK",transformation_type="from_storage",transformer_prefix="k8s:enc:a,b}c:"} 42"#;

        assert_eq!(
            parse_secret_transformations(line).0[&key(Direction::FromStorage, "k8s:enc:a,b}c:")],
            42
        );
    }
}
