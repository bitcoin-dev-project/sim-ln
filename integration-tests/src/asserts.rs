//! Shared assertions over simulation output, independent of which environment produced it.

use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;

use crate::env::NodeHandle;
use crate::runner::SimOutput;
use crate::scenario::ValueSpec;

/// Asserts that the validated activities resolved to the expected source/destination handles, in
/// order. This checks alias/pubkey resolution end to end: however the config referenced the node,
/// validation must land on the right pubkey.
pub fn assert_activities_resolved(out: &SimOutput, expected: &[(&NodeHandle, &NodeHandle)]) {
    assert_eq!(
        out.activities.len(),
        expected.len(),
        "expected {} validated activities, got {}",
        expected.len(),
        out.activities.len()
    );

    for (activity, (source, dest)) in out.activities.iter().zip(expected) {
        assert_eq!(
            activity.source.pubkey, source.pubkey,
            "activity source resolved to {} instead of {} ({})",
            activity.source.pubkey, source.pubkey, source.alias
        );
        assert_eq!(
            activity.destination.pubkey, dest.pubkey,
            "activity destination resolved to {} instead of {} ({})",
            activity.destination.pubkey, dest.pubkey, dest.alias
        );
    }
}

/// Asserts that exactly `expected` payments were dispatched, and that the results CSV recorded
/// every one of them.
pub fn assert_total_payments(out: &SimOutput, expected: u64) {
    assert_eq!(
        out.total_payments, expected,
        "expected {expected} payments, simulation reported {}",
        out.total_payments
    );
    assert_eq!(
        out.records.len() as u64,
        expected,
        "expected {expected} payment records in results CSV, found {}",
        out.records.len()
    );
}

/// Asserts that at least one payment was dispatched and that the CSV agrees with the simulation's
/// own count. Used for random activity, where exact counts depend on the generator.
pub fn assert_payments_dispatched(out: &SimOutput) {
    assert!(
        out.total_payments > 0,
        "expected the simulation to dispatch payments, got none"
    );
    assert_eq!(
        out.records.len() as u64,
        out.total_payments,
        "results CSV has {} records but simulation reported {} payments",
        out.records.len(),
        out.total_payments
    );
}

/// Asserts that every recorded payment succeeded.
pub fn assert_all_success(out: &SimOutput) {
    let failures: Vec<_> = out.records.iter().filter(|r| !r.is_success()).collect();
    assert!(
        failures.is_empty(),
        "expected all payments to succeed, {} failed: {failures:?} (success rate {:.2}%)",
        failures.len(),
        out.success_rate
    );
}

/// Asserts that every recorded payment amount could have been produced by the given spec.
pub fn assert_amounts_within(out: &SimOutput, spec: ValueSpec) {
    for record in &out.records {
        assert!(
            spec.contains(record.amount_msat),
            "payment of {} msat outside configured amount {spec:?}",
            record.amount_msat
        );
    }
}

/// Asserts the results of count-bounded defined activity: payments flow only between the
/// configured pairs, every recorded payment succeeded, and each pair recorded either `count` or
/// `count - 1` payments.
///
/// The final payment of a run is allowed to be missing because meeting a payment count shuts the
/// simulation down in the same instant as the last dispatch, and the results consumer prefers the
/// shutdown signal over draining pending results — so the last payment's record can be dropped.
pub fn assert_defined_payments(out: &SimOutput, pairs: &[(&NodeHandle, &NodeHandle)], count: u64) {
    assert_all_success(out);
    let counts = assert_payments_between(out, pairs);

    for (source, dest) in pairs {
        let recorded = counts
            .get(&(source.pubkey, dest.pubkey))
            .copied()
            .unwrap_or(0);
        assert!(
            (count - 1..=count).contains(&recorded),
            "expected {count} (or {} with the trailing record lost to shutdown) payments from {} \
             to {}, recorded {recorded}",
            count - 1,
            source.alias,
            dest.alias
        );
    }
}

/// Asserts that recorded payments flow only between the expected (source, destination) pairs, and
/// returns the per-pair counts for further assertions.
pub fn assert_payments_between(
    out: &SimOutput,
    pairs: &[(&NodeHandle, &NodeHandle)],
) -> HashMap<(PublicKey, PublicKey), u64> {
    let allowed: Vec<(PublicKey, PublicKey)> =
        pairs.iter().map(|(s, d)| (s.pubkey, d.pubkey)).collect();
    let mut counts: HashMap<(PublicKey, PublicKey), u64> = HashMap::new();

    for record in &out.records {
        let pair = (record.source, record.destination);
        assert!(
            allowed.contains(&pair),
            "payment from {} to {} not part of any configured activity",
            record.source,
            record.destination
        );
        *counts.entry(pair).or_default() += 1;
    }

    counts
}

/// Asserts that no recorded payment involves any of the given nodes, as sender or receiver.
pub fn assert_not_involved(out: &SimOutput, excluded: &[&NodeHandle]) {
    for record in &out.records {
        for node in excluded {
            assert!(
                record.source != node.pubkey && record.destination != node.pubkey,
                "excluded node {} ({}) took part in payment {} -> {}",
                node.alias,
                node.pubkey,
                record.source,
                record.destination
            );
        }
    }
}

/// Asserts that every payment source is one of the given controlled nodes.
pub fn assert_sources_controlled(out: &SimOutput, controlled: &[NodeHandle]) {
    for record in &out.records {
        assert!(
            controlled.iter().any(|n| n.pubkey == record.source),
            "payment source {} is not a controlled node",
            record.source
        );
    }
}
