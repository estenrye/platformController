use crate::etcd_encryption::{target_prefix, EncryptionPhase, EtcdEncryptionSpec, NodeStatus, RewriteMode};
use std::collections::{BTreeMap, BTreeSet};

/// Which provider an apiserver *writes* Secrets with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writer {
    Target,
    /// Writes happened but with other prefixes (or the target plus others).
    Other(Vec<String>),
    /// Could not be determined; the reason says why.
    Unverifiable(String),
}

/// What one apiserver *read* while listing every Secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reader {
    pub listed: i64,
    pub reads: BTreeMap<String, i64>,
}

impl Reader {
    /// Every listed object was decrypted at least once. A list served from the
    /// watch cache decrypts nothing, so it is never complete.
    pub fn complete(&self) -> bool {
        self.reads.values().sum::<i64>() >= self.listed
    }

    pub fn legacy_reads(&self, target: &str) -> i64 {
        self.reads.iter().filter(|(prefix, _)| prefix.as_str() != target).map(|(_, v)| *v).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeEvidence {
    pub name: String,
    pub address: String,
    /// `kms-providers` is present and ok in `/readyz?verbose`.
    pub readyz_kms: bool,
    pub writer: Writer,
    /// `None` when the reader check did not run or failed.
    pub reader: Option<Reader>,
    pub reader_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    pub phase: EncryptionPhase,
    /// Run one rewrite pass this reconcile.
    pub run_rewrite: bool,
    pub reason: String,
    pub nodes: Vec<NodeStatus>,
    pub legacy_prefixes: Vec<String>,
}

fn node_clean(node: &NodeEvidence, target: &str) -> bool {
    node.writer == Writer::Target
        && node.reader.as_ref().is_some_and(|r| r.complete() && r.legacy_reads(target) == 0)
}

/// Every node writes with the target and read every Secret with zero legacy
/// reads. False for no nodes.
pub fn all_clean(nodes: &[NodeEvidence], target: &str) -> bool {
    !nodes.is_empty() && nodes.iter().all(|n| node_clean(n, target))
}

fn node_status(node: &NodeEvidence, target: &str) -> NodeStatus {
    let (writer_prefix, mut reason) = match &node.writer {
        Writer::Target => (Some(target.to_string()), String::new()),
        Writer::Other(prefixes) => (prefixes.first().cloned(), format!("writes with {prefixes:?}, not the target")),
        Writer::Unverifiable(why) => (None, why.clone()),
    };
    let (reads_by_prefix, secrets_listed) = match &node.reader {
        Some(r) => (r.reads.clone(), r.listed),
        None => (BTreeMap::new(), 0),
    };
    if let Some(r) = &node.reader {
        if !r.complete() {
            reason = "cannot verify reads: fewer objects were decrypted than listed".to_string();
        } else if r.legacy_reads(target) > 0 {
            reason = format!("{} legacy reads", r.legacy_reads(target));
        }
    }
    if let Some(err) = &node.reader_error {
        reason = format!("cannot verify reads: {err}");
    }
    NodeStatus {
        name: node.name.clone(),
        address: node.address.clone(),
        verified: node_clean(node, target),
        writer_prefix,
        reads_by_prefix,
        secrets_listed,
        reason,
    }
}

fn derivation(
    phase: EncryptionPhase,
    run_rewrite: bool,
    reason: impl Into<String>,
    statuses: Vec<NodeStatus>,
    legacy: Vec<String>,
) -> Derivation {
    Derivation { phase, run_rewrite, reason: reason.into(), nodes: statuses, legacy_prefixes: legacy }
}

/// The phase, derived from this reconcile's evidence and the spec alone. No
/// remembered state, so nothing can regress or go stale.
pub fn derive(spec: &EtcdEncryptionSpec, nodes: &[NodeEvidence], canary_ok: bool) -> Derivation {
    use EncryptionPhase::*;
    let target = target_prefix(&spec.kms_provider_name);
    let statuses: Vec<NodeStatus> = nodes.iter().map(|n| node_status(n, &target)).collect();
    let legacy: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.reader.as_ref())
        .flat_map(|r| r.reads.iter())
        .filter(|(prefix, count)| prefix.as_str() != target && **count > 0)
        .map(|(prefix, _)| prefix.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let out = |phase, run_rewrite, reason: &str| {
        derivation(phase, run_rewrite, reason, statuses.clone(), legacy.clone())
    };

    if nodes.is_empty() {
        return out(Observing, false, "no control-plane nodes found");
    }
    if let Some((node, why)) = nodes.iter().find_map(|n| match &n.writer {
        Writer::Unverifiable(why) => Some((n, why)),
        _ => None,
    }) {
        return out(Observing, false, &format!("cannot verify node {}: {why}", node.name));
    }

    let writers_target = nodes.iter().filter(|n| n.writer == Writer::Target).count();
    if writers_target == 0 {
        return if nodes.iter().any(|n| n.readyz_kms) {
            out(
                Observing,
                false,
                &format!(
                    "a KMS provider is configured but the apiserver is not writing with {target}: check \
                     spec.kmsProviderName and that the KMS provider is listed first"
                ),
            )
        } else {
            out(
                NotConfigured,
                false,
                "no KMS provider is active; this controller does not enable KMS (see the runbook)",
            )
        };
    }
    if writers_target < nodes.len() {
        return out(Observing, false, "mixed: some apiservers do not write with the target provider yet (a rolling restart?)");
    }

    // Every apiserver writes with the target provider.
    if all_clean(nodes, &target) {
        return if spec.acknowledgements.legacy_providers_removed && canary_ok {
            out(
                Verified,
                false,
                "every apiserver reads and writes only with the target provider, and you acknowledged \
                 removing the legacy providers (metrics cannot prove the config no longer lists them)",
            )
        } else if spec.acknowledgements.legacy_providers_removed {
            out(ReadyToRemoveLegacy, false, "the canary Secret did not round-trip; not verified")
        } else {
            out(
                ReadyToRemoveLegacy,
                false,
                "no Secret is stored under a legacy provider on any apiserver: it is safe to remove the \
                 legacy providers from the EncryptionConfiguration, then set \
                 acknowledgements.legacyProvidersRemoved",
            )
        };
    }

    let legacy_seen = nodes
        .iter()
        .filter_map(|n| n.reader.as_ref())
        .any(|r| r.complete() && r.legacy_reads(&target) > 0);
    if legacy_seen && spec.rewrite == RewriteMode::Enabled {
        return out(Migrating, true, "rewriting every Secret through the target provider");
    }
    if legacy_seen {
        return out(Observing, false, "legacy objects remain; set rewrite: Enabled to migrate");
    }
    out(Observing, false, "cannot verify reads on every apiserver; not rewriting")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::PlatformKind;
    use crate::etcd_encryption::{Acknowledgements, EncryptionPhase::*};

    const TARGET: &str = "k8s:enc:kms:v2:barbican:";
    const SECRETBOX: &str = "k8s:enc:secretbox:v1:";

    fn spec(rewrite: RewriteMode, acked: bool) -> EtcdEncryptionSpec {
        EtcdEncryptionSpec {
            platform_kind: PlatformKind::TalosLinux,
            kms_provider_name: "barbican".to_string(),
            rewrite,
            acknowledgements: Acknowledgements { legacy_providers_removed: acked },
        }
    }

    fn reader(listed: i64, kms: i64, secretbox: i64) -> Option<Reader> {
        let mut reads = BTreeMap::new();
        if kms > 0 {
            reads.insert(TARGET.to_string(), kms);
        }
        if secretbox > 0 {
            reads.insert(SECRETBOX.to_string(), secretbox);
        }
        Some(Reader { listed, reads })
    }

    fn node(name: &str, writer: Writer, reader: Option<Reader>) -> NodeEvidence {
        NodeEvidence {
            name: name.to_string(),
            address: "10.0.0.1".to_string(),
            readyz_kms: true,
            writer,
            reader,
            reader_error: None,
        }
    }

    fn clean(name: &str) -> NodeEvidence {
        node(name, Writer::Target, reader(10, 10, 0))
    }

    fn dirty(name: &str) -> NodeEvidence {
        node(name, Writer::Target, reader(10, 7, 3))
    }

    #[test]
    fn no_nodes_is_observing() {
        let d = derive(&spec(RewriteMode::Enabled, false), &[], false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
    }

    #[test]
    fn an_unverifiable_node_blocks_everything_even_when_the_others_are_clean() {
        // Review Focus 1.
        let nodes = [clean("a"), node("b", Writer::Unverifiable("tls: bad certificate".into()), None), clean("c")];

        let d = derive(&spec(RewriteMode::Enabled, true), &nodes, true);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert!(d.reason.contains("b") && d.reason.contains("tls: bad certificate"), "{}", d.reason);
        assert!(!d.nodes.iter().find(|n| n.name == "b").unwrap().verified);
    }

    #[test]
    fn no_writer_is_the_target_and_no_kms_reported_means_not_configured() {
        let mut a = node("a", Writer::Other(vec![SECRETBOX.to_string()]), None);
        a.readyz_kms = false;

        assert_eq!(derive(&spec(RewriteMode::Disabled, false), &[a], false).phase, NotConfigured);
    }

    #[test]
    fn kms_reported_but_not_writing_with_the_target_is_observing_with_a_hint() {
        let a = node("a", Writer::Other(vec![SECRETBOX.to_string()]), None);

        let d = derive(&spec(RewriteMode::Enabled, false), &[a], false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert!(d.reason.contains("kmsProviderName"), "{}", d.reason);
    }

    #[test]
    fn mixed_writers_never_migrate() {
        // Review Focus 2: a rolling restart; a rewrite now could store Secrets under the old provider.
        let nodes = [dirty("a"), node("b", Writer::Other(vec![SECRETBOX.to_string()]), None), dirty("c")];

        let d = derive(&spec(RewriteMode::Enabled, false), &nodes, false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert!(d.reason.contains("mixed"), "{}", d.reason);
    }

    #[test]
    fn every_node_clean_is_ready_to_remove_legacy_until_acknowledged() {
        let nodes = [clean("a"), clean("b"), clean("c")];

        let d = derive(&spec(RewriteMode::Disabled, false), &nodes, false);

        assert_eq!(d.phase, ReadyToRemoveLegacy);
        assert!(d.nodes.iter().all(|n| n.verified));
        assert!(d.legacy_prefixes.is_empty());
    }

    #[test]
    fn verified_needs_the_acknowledgement_and_a_round_tripping_canary() {
        let nodes = [clean("a"), clean("b")];

        assert_eq!(derive(&spec(RewriteMode::Disabled, true), &nodes, true).phase, Verified);
        assert_eq!(derive(&spec(RewriteMode::Disabled, true), &nodes, false).phase, ReadyToRemoveLegacy);
        assert_eq!(derive(&spec(RewriteMode::Disabled, false), &nodes, true).phase, ReadyToRemoveLegacy);
    }

    #[test]
    fn legacy_reads_with_rewrite_disabled_only_observe() {
        let nodes = [dirty("a"), clean("b")];

        let d = derive(&spec(RewriteMode::Disabled, false), &nodes, false);

        assert_eq!(d.phase, Observing);
        assert!(!d.run_rewrite);
        assert_eq!(d.legacy_prefixes, vec![SECRETBOX.to_string()]);
        assert!(d.reason.contains("rewrite: Enabled"), "{}", d.reason);
    }

    #[test]
    fn legacy_reads_with_rewrite_enabled_and_every_writer_the_target_migrate() {
        // Review Focus 6.
        let nodes = [dirty("a"), clean("b"), clean("c")];

        let d = derive(&spec(RewriteMode::Enabled, false), &nodes, false);

        assert_eq!(d.phase, Migrating);
        assert!(d.run_rewrite);
    }

    #[test]
    fn a_cache_served_list_is_incomplete_and_never_counts_as_clean() {
        // Review Focus 3: 10 listed but only 4 decrypts seen.
        let nodes = [node("a", Writer::Target, reader(10, 4, 0)), clean("b")];

        assert!(!all_clean(&nodes, TARGET));
        let d = derive(&spec(RewriteMode::Enabled, true), &nodes, true);
        assert_ne!(d.phase, Verified);
        assert_ne!(d.phase, ReadyToRemoveLegacy);
    }

    #[test]
    fn an_incomplete_or_unverifiable_reader_never_triggers_a_rewrite() {
        let incomplete = [node("a", Writer::Target, reader(10, 4, 0))];
        let mut errored = node("a", Writer::Target, None);
        errored.reader_error = Some("list timed out".to_string());

        for nodes in [&incomplete[..], &[errored][..]] {
            let d = derive(&spec(RewriteMode::Enabled, false), nodes, false);

            assert_eq!(d.phase, Observing);
            assert!(!d.run_rewrite);
            assert!(d.reason.contains("cannot verify reads"), "{}", d.reason);
        }
    }

    #[test]
    fn an_identity_reading_is_legacy() {
        let mut reads = BTreeMap::new();
        reads.insert(TARGET.to_string(), 8);
        reads.insert(String::new(), 2);
        let nodes = [node("a", Writer::Target, Some(Reader { listed: 10, reads }))];

        assert!(!all_clean(&nodes, TARGET));
        assert_eq!(derive(&spec(RewriteMode::Disabled, false), &nodes, false).legacy_prefixes, vec![String::new()]);
    }

    #[test]
    fn all_clean_is_false_for_no_nodes() {
        assert!(!all_clean(&[], TARGET));
    }

    #[test]
    fn node_status_reports_the_writer_prefix_and_reads() {
        let d = derive(&spec(RewriteMode::Disabled, false), &[dirty("a")], false);
        let n = &d.nodes[0];

        assert_eq!(n.writer_prefix.as_deref(), Some(TARGET));
        assert_eq!(n.secrets_listed, 10);
        assert_eq!(n.reads_by_prefix[SECRETBOX], 3);
        assert!(!n.verified);
    }
}
