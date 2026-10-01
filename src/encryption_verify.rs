use crate::apiserver_probe::{ApiserverProbe, NodeTarget, ProbeError};
use crate::encryption_verdict::{NodeEvidence, Reader, Writer};
use crate::transformation_metrics::{delta, parse_secret_transformations, Delta};

fn counters(body: &str) -> crate::transformation_metrics::Transformations {
    parse_secret_transformations(body)
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

async fn read_check<P: ApiserverProbe>(probe: &P, node: &NodeTarget) -> Result<Reader, String> {
    let before = counters(&probe.metrics(node).await.map_err(|e| e.to_string())?);
    let listed = probe.list_all_secrets(node).await.map_err(|e| e.to_string())?;
    let after = counters(&probe.metrics(node).await.map_err(|e| e.to_string())?);
    let delta = delta(&before, &after)
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

    let readyz_kms = match probe.readyz_kms(node).await {
        Ok(v) => v,
        Err(err) => return unverifiable(false, err.to_string()),
    };

    let before = match probe.metrics(node).await {
        Ok(body) => counters(&body),
        Err(err) => return unverifiable(readyz_kms, err.to_string()),
    };
    if let Err(err) = probe.canary_write(node).await {
        return unverifiable(readyz_kms, format!("canary write failed: {err}"));
    }
    let after = match probe.metrics(node).await {
        Ok(body) => counters(&body),
        Err(err) => return unverifiable(readyz_kms, err.to_string()),
    };

    let writer = writer_from(delta(&before, &after), target);
    if writer != Writer::Target {
        return evidence(readyz_kms, writer, None, None);
    }
    match read_check(probe, node).await {
        Ok(reader) => evidence(readyz_kms, writer, Some(reader), None),
        Err(err) => evidence(readyz_kms, writer, None, Some(err)),
    }
}

/// Verifies every control-plane apiserver, in discovery order. A discovery error
/// is an error: an empty result must never be mistaken for "no nodes to worry about".
pub async fn verify_cluster<P: ApiserverProbe>(probe: &P, target: &str) -> Result<Vec<NodeEvidence>, ProbeError> {
    let nodes = probe.control_plane_nodes().await?;
    let mut evidence = Vec::with_capacity(nodes.len());
    for node in &nodes {
        evidence.push(verify_node(probe, node, target).await);
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

    fn metrics(rows: &[(&str, &str, i64)]) -> String {
        rows.iter()
            .map(|(dir, prefix, n)| {
                format!(
                    "apiserver_storage_transformation_operations_total{{resource=\"secrets\",status=\"OK\",transformation_type=\"{dir}\",transformer_prefix=\"{prefix}\"}} {n}\n"
                )
            })
            .collect()
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
        scripts: Mutex<HashMap<String, Script>>,
    }

    fn target(name: &str) -> NodeTarget {
        NodeTarget { name: name.to_string(), address: format!("10.0.0.{}", name.len()) }
    }

    impl Fake {
        fn with(scripts: Vec<(&str, Script)>) -> Fake {
            Fake {
                nodes: Ok(scripts.iter().map(|(n, _)| target(n)).collect()),
                scripts: Mutex::new(scripts.into_iter().map(|(n, s)| (n.to_string(), s)).collect()),
            }
        }
    }

    impl ApiserverProbe for Fake {
        async fn control_plane_nodes(&self) -> Result<Vec<NodeTarget>, ProbeError> {
            self.nodes.clone().map_err(ProbeError::Request)
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

    #[tokio::test]
    async fn verify_cluster_checks_every_discovered_node() {
        let fake = Fake::with(vec![("cp1", healthy(10, 0, 10)), ("cp22", healthy(10, 0, 10))]);

        let evidence = verify_cluster(&fake, TARGET).await.unwrap();

        assert_eq!(evidence.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["cp1", "cp22"]);
        assert!(evidence.iter().all(|e| e.writer == Writer::Target));
    }

    #[tokio::test]
    async fn a_node_discovery_error_is_an_error_not_an_empty_result() {
        let fake = Fake { nodes: Err("list nodes: forbidden".to_string()), scripts: Mutex::new(HashMap::new()) };

        assert!(verify_cluster(&fake, TARGET).await.is_err());
    }
}
