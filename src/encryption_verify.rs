use crate::apiserver_probe::{same_address, ApiserverProbe, NodeTarget, ProbeError};
use crate::encryption_verdict::{NodeEvidence, Reader, Writer};
use crate::transformation_metrics::{delta, parse_process_start_time, parse_secret_transformations, Delta, Transformations};

const RESTARTED: &str = "the apiserver restarted (or exposes no process_start_time_seconds) during the check";

/// One `/metrics` snapshot: the Secrets counters and the process start time.
struct Snapshot {
    counters: Transformations,
    started: Option<f64>,
}

fn snapshot(body: &str) -> Snapshot {
    Snapshot { counters: parse_secret_transformations(body), started: parse_process_start_time(body) }
}

/// The start time shared by both snapshots, or `None` if either lacks one or
/// they differ: the apiserver restarted between them, so a counter that only
/// rose since the restart could make the delta look plausible.
fn same_process(before: &Snapshot, after: &Snapshot) -> Option<f64> {
    match (before.started, after.started) {
        (Some(b), Some(a)) if a == b => Some(b),
        _ => None,
    }
}

/// Which provider this apiserver wrote with, from the `to_storage` delta around
/// one canary write.
fn writer_from(delta: Option<Delta>, target: &str) -> Writer {
    let Some(delta) = delta else {
        return Writer::Unverifiable("a counter went backwards (the apiserver restarted mid-check)".to_string());
    };
    let others: Vec<String> = delta.to_storage.keys().filter(|p| p.as_str() != target).cloned().collect();
    if !others.is_empty() {
        return Writer::Other(others);
    }
    if delta.to_storage.get(target).copied().unwrap_or(0) > 0 {
        Writer::Target
    } else {
        Writer::Unverifiable("the canary write was not counted by any provider".to_string())
    }
}

/// The reader check. `writer_started` is the process start time seen by the
/// writer check: the reader must run against the same apiserver process.
async fn read_check<P: ApiserverProbe>(probe: &P, node: &NodeTarget, writer_started: f64) -> Result<Reader, String> {
    let before = snapshot(&probe.metrics(node).await.map_err(|e| e.to_string())?);
    let listed = probe.list_all_secrets(node).await.map_err(|e| e.to_string())?;
    let after = snapshot(&probe.metrics(node).await.map_err(|e| e.to_string())?);
    if same_process(&before, &after) != Some(writer_started) {
        return Err(RESTARTED.to_string());
    }
    let delta = delta(&before.counters, &after.counters)
        .ok_or_else(|| "a counter went backwards (the apiserver restarted mid-check)".to_string())?;
    let listed = i64::try_from(listed).map_err(|_| format!("listed count {listed} does not fit a counter"))?;
    Ok(Reader { listed, reads: delta.from_storage })
}

/// Verifies one apiserver. Any failure leaves the node unverified with the
/// reason; nothing here can turn an error into a clean result.
pub async fn verify_node<P: ApiserverProbe>(probe: &P, node: &NodeTarget, target: &str) -> NodeEvidence {
    let evidence = |readyz_kms, writer, reader, reader_error| NodeEvidence {
        name: node.name.clone(),
        address: node.address.clone(),
        readyz_kms,
        writer,
        reader,
        reader_error,
    };
    let unverifiable = |readyz_kms: bool, why: String| evidence(readyz_kms, Writer::Unverifiable(why), None, None);

    // A control-plane node with no InternalIP cannot be reached directly; it is
    // still an apiserver this controller cannot vouch for.
    if node.address.is_empty() {
        return unverifiable(false, "node has no InternalIP".to_string());
    }

    let readyz_kms = match probe.readyz_kms(node).await {
        Ok(v) => v,
        Err(err) => return unverifiable(false, err.to_string()),
    };

    let before = match probe.metrics(node).await {
        Ok(body) => snapshot(&body),
        Err(err) => return unverifiable(readyz_kms, err.to_string()),
    };
    if let Err(err) = probe.canary_write(node).await {
        return unverifiable(readyz_kms, format!("canary write failed: {err}"));
    }
    let after = match probe.metrics(node).await {
        Ok(body) => snapshot(&body),
        Err(err) => return unverifiable(readyz_kms, err.to_string()),
    };
    let Some(started) = same_process(&before, &after) else {
        return unverifiable(readyz_kms, RESTARTED.to_string());
    };

    let writer = writer_from(delta(&before.counters, &after.counters), target);
    if writer != Writer::Target {
        return evidence(readyz_kms, writer, None, None);
    }
    match read_check(probe, node, started).await {
        Ok(reader) => evidence(readyz_kms, writer, Some(reader), None),
        Err(err) => evidence(readyz_kms, writer, None, Some(err)),
    }
}

/// The reason given for an apiserver registered in the `kubernetes` Endpoints
/// that matches no discovered control-plane node.
pub const UNDISCOVERED_APISERVER: &str =
    "an apiserver registered in the kubernetes Endpoints is not a discovered control-plane node";

/// Verifies every control-plane apiserver, in discovery order, then
/// cross-checks the `default/kubernetes` EndpointSlice addresses: an address
/// that matches no discovered node is an apiserver this controller did not
/// check, so it is added as unverifiable evidence. A discovery or endpoints
/// error is an error: an empty result must never be mistaken for "no nodes to
/// worry about".
pub async fn verify_cluster<P: ApiserverProbe>(probe: &P, target: &str) -> Result<Vec<NodeEvidence>, ProbeError> {
    let nodes = probe.control_plane_nodes().await?;
    let mut evidence = Vec::with_capacity(nodes.len());
    for node in &nodes {
        evidence.push(verify_node(probe, node, target).await);
    }
    for addr in probe.apiserver_endpoint_addresses().await? {
        if nodes.iter().any(|n| same_address(&n.address, &addr)) {
            continue;
        }
        evidence.push(NodeEvidence {
            name: format!("endpoint {addr}"),
            address: addr,
            readyz_kms: false,
            writer: Writer::Unverifiable(UNDISCOVERED_APISERVER.to_string()),
            reader: None,
            reader_error: None,
        });
    }
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const TARGET: &str = "k8s:enc:kms:v2:barbican:";
    const SECRETBOX: &str = "k8s:enc:secretbox:v1:";

    const STARTED: f64 = 1_700_000_000.0;

    /// A `/metrics` body from an apiserver that started at `STARTED`.
    fn metrics(rows: &[(&str, &str, i64)]) -> String {
        metrics_started(Some(STARTED), rows)
    }

    /// A `/metrics` body with the given `process_start_time_seconds` (`None`: absent).
    fn metrics_started(started: Option<f64>, rows: &[(&str, &str, i64)]) -> String {
        let start_line = started.map(|t| format!("process_start_time_seconds {t:e}\n")).unwrap_or_default();
        start_line
            + &rows
                .iter()
            .map(|(dir, prefix, n)| {
                format!(
                    "apiserver_storage_transformation_operations_total{{resource=\"secrets\",status=\"OK\",transformation_type=\"{dir}\",transformer_prefix=\"{prefix}\"}} {n}\n"
                )
            })
            .collect::<String>()
    }

    #[derive(Default)]
    struct Script {
        readyz: Option<Result<bool, String>>,
        metrics: Vec<Result<String, String>>,
        canary_write: Option<Result<(), String>>,
        list: Option<Result<u64, String>>,
    }

    struct Fake {
        nodes: Result<Vec<NodeTarget>, String>,
        endpoints: Result<Vec<String>, String>,
        scripts: Mutex<HashMap<String, Script>>,
    }

    fn target(name: &str) -> NodeTarget {
        NodeTarget { name: name.to_string(), address: format!("10.0.0.{}", name.len()) }
    }

    impl Fake {
        fn with(scripts: Vec<(&str, Script)>) -> Fake {
            let nodes: Vec<NodeTarget> = scripts.iter().map(|(n, _)| target(n)).collect();
            Fake {
                endpoints: Ok(nodes.iter().map(|n| n.address.clone()).collect()),
                nodes: Ok(nodes),
                scripts: Mutex::new(scripts.into_iter().map(|(n, s)| (n.to_string(), s)).collect()),
            }
        }
    }

    impl ApiserverProbe for Fake {
        async fn control_plane_nodes(&self) -> Result<Vec<NodeTarget>, ProbeError> {
            self.nodes.clone().map_err(ProbeError::Request)
        }
        async fn apiserver_endpoint_addresses(&self) -> Result<Vec<String>, ProbeError> {
            self.endpoints.clone().map_err(ProbeError::Request)
        }
        async fn readyz_kms(&self, node: &NodeTarget) -> Result<bool, ProbeError> {
            self.scripts.lock().unwrap().get_mut(&node.name).unwrap().readyz.take().unwrap().map_err(ProbeError::Request)
        }
        async fn metrics(&self, node: &NodeTarget) -> Result<String, ProbeError> {
            let mut scripts = self.scripts.lock().unwrap();
            let s = scripts.get_mut(&node.name).unwrap();
            assert!(!s.metrics.is_empty(), "unexpected extra metrics call on {}", node.name);
            s.metrics.remove(0).map_err(ProbeError::Request)
        }
        async fn canary_write(&self, node: &NodeTarget) -> Result<(), ProbeError> {
            self.scripts.lock().unwrap().get_mut(&node.name).unwrap().canary_write.take().unwrap().map_err(ProbeError::Request)
        }
        async fn list_all_secrets(&self, node: &NodeTarget) -> Result<u64, ProbeError> {
            self.scripts.lock().unwrap().get_mut(&node.name).unwrap().list.take().unwrap().map_err(ProbeError::Request)
        }
        async fn canary_round_trip(&self) -> Result<bool, ProbeError> {
            Ok(true)
        }
    }

    /// A healthy node: writes with the target; the list decrypts 7 target + 3 secretbox of 10.
    fn healthy(reads_kms: i64, reads_secretbox: i64, listed: u64) -> Script {
        Script {
            readyz: Some(Ok(true)),
            metrics: vec![
                Ok(metrics(&[("to_storage", TARGET, 1), ("from_storage", TARGET, 100), ("from_storage", SECRETBOX, 50)])),
                Ok(metrics(&[("to_storage", TARGET, 2), ("from_storage", TARGET, 100), ("from_storage", SECRETBOX, 50)])),
                Ok(metrics(&[("to_storage", TARGET, 2), ("from_storage", TARGET, 100), ("from_storage", SECRETBOX, 50)])),
                Ok(metrics(&[
                    ("to_storage", TARGET, 2),
                    ("from_storage", TARGET, 100 + reads_kms),
                    ("from_storage", SECRETBOX, 50 + reads_secretbox),
                ])),
            ],
            canary_write: Some(Ok(())),
            list: Some(Ok(listed)),
        }
    }

    #[tokio::test]
    async fn a_healthy_node_writes_with_the_target_and_reports_its_reads() {
        let fake = Fake::with(vec![("cp1", healthy(7, 3, 10))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.readyz_kms);
        let r = e.reader.expect("reader ran");
        assert_eq!(r.listed, 10);
        assert_eq!(r.reads[TARGET], 7);
        assert_eq!(r.reads[SECRETBOX], 3);
        assert!(r.complete());
    }

    #[tokio::test]
    async fn a_cache_served_list_decrypts_fewer_objects_than_listed() {
        // Review Focus 3.
        let fake = Fake::with(vec![("cp1", healthy(2, 0, 10))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(!e.reader.unwrap().complete());
    }

    #[tokio::test]
    async fn a_node_writing_with_another_provider_is_not_the_target_and_skips_the_reader() {
        let mut s = healthy(7, 3, 10);
        s.metrics[1] = Ok(metrics(&[("to_storage", TARGET, 1), ("to_storage", SECRETBOX, 1), ("from_storage", TARGET, 100)]));
        s.metrics[0] = Ok(metrics(&[("to_storage", TARGET, 1), ("from_storage", TARGET, 100)]));
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Other(vec![SECRETBOX.to_string()]));
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn a_counter_reset_between_snapshots_is_unverifiable() {
        // Review Focus 4.
        let mut s = healthy(7, 3, 10);
        s.metrics[1] = Ok(metrics(&[("to_storage", TARGET, 0), ("from_storage", TARGET, 1)]));
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(matches!(e.writer, Writer::Unverifiable(_)), "{:?}", e.writer);
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn no_observed_write_is_unverifiable() {
        let mut s = healthy(7, 3, 10);
        s.metrics[1] = s.metrics[0].clone();
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        assert!(matches!(verify_node(&fake, &target("cp1"), TARGET).await.writer, Writer::Unverifiable(_)));
    }

    #[tokio::test]
    async fn a_readyz_error_makes_the_node_unverifiable_and_stops() {
        // Review Focus 1.
        let s = Script { readyz: Some(Err("tls: bad certificate".to_string())), ..Default::default() };
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(matches!(&e.writer, Writer::Unverifiable(why) if why.contains("bad certificate")));
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn a_canary_write_error_makes_the_node_unverifiable() {
        let mut s = healthy(7, 3, 10);
        s.canary_write = Some(Err("forbidden".to_string()));
        s.metrics.truncate(1);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        assert!(matches!(verify_node(&fake, &target("cp1"), TARGET).await.writer, Writer::Unverifiable(_)));
    }

    #[tokio::test]
    async fn a_list_error_is_a_reader_error_not_a_clean_result() {
        let mut s = healthy(7, 3, 10);
        s.list = Some(Err("timed out".to_string()));
        s.metrics.truncate(3);
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.reader.is_none());
        assert!(e.reader_error.as_deref().is_some_and(|m| m.contains("timed out")));
    }

    /// A healthy script whose metrics snapshots report these start times
    /// (writer before, writer after, reader before, reader after).
    fn with_starts(starts: [Option<f64>; 4]) -> Script {
        let mut s = healthy(10, 0, 10);
        let rows: [&[(&str, &str, i64)]; 4] = [
            &[("to_storage", TARGET, 1), ("from_storage", TARGET, 100)],
            &[("to_storage", TARGET, 2), ("from_storage", TARGET, 100)],
            &[("to_storage", TARGET, 2), ("from_storage", TARGET, 100)],
            &[("to_storage", TARGET, 2), ("from_storage", TARGET, 110)],
        ];
        s.metrics = rows.iter().zip(starts).map(|(r, t)| Ok(metrics_started(t, r))).collect();
        s
    }

    #[tokio::test]
    async fn equal_start_times_throughout_verify_cleanly() {
        let fake = Fake::with(vec![("cp1", with_starts([Some(STARTED); 4]))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.reader_error.is_none(), "{:?}", e.reader_error);
        assert!(e.reader.unwrap().complete());
    }

    #[tokio::test]
    async fn a_restart_during_the_writer_check_is_unverifiable() {
        // Final review I3: counters that only rose after a restart do not decrease.
        let mut s = with_starts([Some(STARTED), Some(STARTED + 60.0), None, None]);
        s.metrics.truncate(2);
        s.list = None;
        let fake = Fake::with(vec![("cp1", s)]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert!(matches!(&e.writer, Writer::Unverifiable(why) if why.contains("restarted")), "{:?}", e.writer);
        assert!(e.reader.is_none());
    }

    #[tokio::test]
    async fn a_missing_start_time_in_the_writer_check_is_unverifiable() {
        for starts in [[None, Some(STARTED), None, None], [Some(STARTED), None, None, None], [None, None, None, None]] {
            let mut s = with_starts(starts);
            s.metrics.truncate(2);
            s.list = None;
            let fake = Fake::with(vec![("cp1", s)]);

            let e = verify_node(&fake, &target("cp1"), TARGET).await;

            assert!(matches!(&e.writer, Writer::Unverifiable(why) if why.contains("process_start_time_seconds")), "{:?}", e.writer);
        }
    }

    #[tokio::test]
    async fn a_restart_during_the_reader_check_is_a_reader_error() {
        let fake = Fake::with(vec![("cp1", with_starts([Some(STARTED), Some(STARTED), Some(STARTED), Some(STARTED + 60.0)]))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.reader.is_none());
        assert!(e.reader_error.as_deref().is_some_and(|m| m.contains("restarted")), "{:?}", e.reader_error);
    }

    #[tokio::test]
    async fn a_restart_between_the_writer_and_reader_checks_is_a_reader_error() {
        let later = Some(STARTED + 60.0);
        let fake = Fake::with(vec![("cp1", with_starts([Some(STARTED), Some(STARTED), later, later]))]);

        let e = verify_node(&fake, &target("cp1"), TARGET).await;

        assert_eq!(e.writer, Writer::Target);
        assert!(e.reader.is_none());
        assert!(e.reader_error.as_deref().is_some_and(|m| m.contains("restarted")), "{:?}", e.reader_error);
    }

    #[tokio::test]
    async fn a_missing_start_time_in_the_reader_check_is_a_reader_error() {
        for starts in [[Some(STARTED), Some(STARTED), None, Some(STARTED)], [Some(STARTED), Some(STARTED), Some(STARTED), None]] {
            let fake = Fake::with(vec![("cp1", with_starts(starts))]);

            let e = verify_node(&fake, &target("cp1"), TARGET).await;

            assert!(e.reader.is_none());
            assert!(e.reader_error.as_deref().is_some_and(|m| m.contains("process_start_time_seconds")), "{:?}", e.reader_error);
        }
    }

    #[tokio::test]
    async fn verify_cluster_checks_every_discovered_node() {
        let fake = Fake::with(vec![("cp1", healthy(10, 0, 10)), ("cp22", healthy(10, 0, 10))]);

        let evidence = verify_cluster(&fake, TARGET).await.unwrap();

        assert_eq!(evidence.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["cp1", "cp22"]);
        assert!(evidence.iter().all(|e| e.writer == Writer::Target));
    }

    #[tokio::test]
    async fn a_node_discovery_error_is_an_error_not_an_empty_result() {
        let fake = Fake {
            nodes: Err("list nodes: forbidden".to_string()),
            endpoints: Ok(vec![]),
            scripts: Mutex::new(HashMap::new()),
        };

        assert!(verify_cluster(&fake, TARGET).await.is_err());
    }

    #[tokio::test]
    async fn a_node_without_an_internal_ip_is_unverifiable_without_any_call() {
        // Final review I1. The empty script panics on any probe call.
        let fake = Fake::with(vec![("cp1", Script::default())]);
        let node = NodeTarget { name: "cp1".to_string(), address: String::new() };

        let e = verify_node(&fake, &node, TARGET).await;

        assert_eq!(e.writer, Writer::Unverifiable("node has no InternalIP".to_string()));
        assert!(e.reader.is_none() && e.reader_error.is_none());
    }

    fn spec() -> crate::etcd_encryption::EtcdEncryptionSpec {
        serde_json::from_value(serde_json::json!({
            "platformKind": "talos-linux",
            "kmsProviderName": "barbican",
            "rewrite": "Enabled",
            "acknowledgements": { "legacyProvidersRemoved": true }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn an_endpoint_address_matching_no_discovered_node_is_unverifiable_evidence() {
        // Final review I1: an apiserver that is not a labelled control-plane node.
        let mut fake = Fake::with(vec![("cp1", healthy(10, 0, 10))]);
        fake.endpoints = Ok(vec!["10.0.0.3".to_string(), "10.0.0.99".to_string()]);

        let evidence = verify_cluster(&fake, TARGET).await.unwrap();

        assert_eq!(evidence.len(), 2);
        let extra = &evidence[1];
        assert_eq!((extra.name.as_str(), extra.address.as_str()), ("endpoint 10.0.0.99", "10.0.0.99"));
        assert!(!extra.readyz_kms && extra.reader.is_none() && extra.reader_error.is_none());
        assert!(matches!(&extra.writer, Writer::Unverifiable(why) if why.contains("not a discovered control-plane node")));
        for canary_ok in [true, false] {
            let d = crate::encryption_verdict::derive(&spec(), &evidence, canary_ok);
            assert_eq!(d.phase, crate::etcd_encryption::EncryptionPhase::Observing, "{}", d.reason);
            assert!(!d.run_rewrite);
        }
    }

    #[tokio::test]
    async fn endpoint_addresses_match_discovered_nodes_by_ip_not_by_spelling() {
        let mut fake = Fake::with(vec![("cp1", healthy(10, 0, 10))]);
        fake.nodes = Ok(vec![NodeTarget { name: "cp1".to_string(), address: "fd00:0:0:0::11".to_string() }]);
        fake.endpoints = Ok(vec!["fd00::11".to_string()]);

        let evidence = verify_cluster(&fake, TARGET).await.unwrap();

        assert_eq!(evidence.len(), 1, "{evidence:?}");
        assert_eq!(evidence[0].writer, Writer::Target);
    }

    #[tokio::test]
    async fn an_endpoints_error_is_an_error() {
        let mut fake = Fake::with(vec![("cp1", healthy(10, 0, 10))]);
        fake.endpoints = Err("list endpointslices: forbidden".to_string());

        assert!(verify_cluster(&fake, TARGET).await.is_err());
    }
}
