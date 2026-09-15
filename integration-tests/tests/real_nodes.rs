//! The real-node tier: a heterogeneous regtest network (LND, CLN, Eclair, ldk-server in a ring
//! of channels over one bitcoind) run in docker, with simulations driven against it through every
//! connector.
//!
//! Everything runs inside one test: container startup dominates runtime, so the network is built
//! once and the scenarios run sequentially against it. Containers are cleaned up when the test
//! ends because the network is dropped here — a shared static would leak them.

use std::time::Duration;

use anyhow::ensure;
use integration_tests::asserts::{
    assert_activities_resolved, assert_defined_payments, assert_payments_dispatched,
    assert_sources_controlled,
};
use integration_tests::env::containers::RealNetwork;
use integration_tests::env::TestNetwork;
use integration_tests::runner::{run_real, RunOptions, SimFile, SimOutput};
use integration_tests::scenario::{ConfigStyle, NodeRef, Scenario, ValueSpec};
use sim_cli::parsing::NodeConnection;

/// Ring order: 0 = LND, 1 = CLN, 2 = Eclair, 3 = ldk-server; channels 0->1->2->3->0.
const RING: [(usize, usize); 4] = [(0, 1), (1, 2), (2, 3), (3, 0)];

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker; run with make integration-real"]
async fn real_nodes() {
    let creds_dir = tempfile::tempdir().expect("create credentials dir");

    eprintln!("=== starting heterogeneous regtest network (bitcoind + lnd/cln/eclair/ldk) ===");
    let network = RealNetwork::start(creds_dir.path())
        .await
        .expect("real node network should start");
    eprintln!("=== network ready ===");

    run_scenario(
        "defined keysends around the ring (every connector sends and receives)",
        &network,
        scenario_ring_defined(&network),
    )
    .await;
    run_scenario(
        "multi-hop defined payments (lnd -> cln -> eclair)",
        &network,
        scenario_multi_hop(&network),
    )
    .await;
    run_scenario(
        "alias-referenced defined payments (cln -> ldk)",
        &network,
        scenario_alias_refs(&network),
    )
    .await;
    run_scenario(
        "random activity across all four implementations",
        &network,
        scenario_random(&network),
    )
    .await;
}

/// Runs one scenario with a hard timeout, dumping every container's logs when it errors so CI
/// output includes the node-side view. (Assertion panics inside scenarios point at sim-side
/// state, which the panic message already carries.)
async fn run_scenario(
    name: &str,
    network: &RealNetwork,
    scenario: impl std::future::Future<Output = anyhow::Result<()>>,
) {
    eprintln!("=== scenario: {name} ===");
    match tokio::time::timeout(Duration::from_secs(600), scenario).await {
        Ok(Ok(())) => eprintln!("=== scenario passed: {name} ==="),
        Ok(Err(e)) => {
            network.dump_all_logs().await;
            panic!("scenario failed: {name}: {e:#}");
        },
        Err(_) => {
            network.dump_all_logs().await;
            panic!("scenario timed out: {name}");
        },
    }
}

/// Writes the scenario to a sim file and runs it against the network.
async fn run(
    network: &RealNetwork,
    scenario: &Scenario,
    opts: RunOptions,
) -> anyhow::Result<SimOutput> {
    let dir = tempfile::tempdir()?;
    let sim_file = SimFile::new(network.config_fragment())
        .scenario(scenario, network.nodes())
        .write(dir.path())?;
    run_real(&sim_file, opts).await
}

/// One defined activity per directed ring edge: proves every connector both dispatches and
/// receives keysend cross-implementation, with receipt confirmed against each destination node's
/// own books rather than sim-ln's records alone.
async fn scenario_ring_defined(network: &RealNetwork) -> anyhow::Result<()> {
    let count = 3u64;

    let mut receipts_before = [0u64; 4];
    for (i, receipts) in receipts_before.iter_mut().enumerate() {
        *receipts = network.settled_keysend_count(i).await?;
    }

    let scenario = Scenario::Defined {
        pairs: RING.to_vec(),
        count: Some(count),
        style: ConfigStyle {
            amount_msat: ValueSpec::Scalar(10_000),
            interval_secs: ValueSpec::Scalar(2),
            ..Default::default()
        },
    };
    let out = run(
        network,
        &scenario,
        RunOptions {
            total_time: Some(180),
            ..Default::default()
        },
    )
    .await?;

    // The untagged `nodes` config section must infer each connector implementation from its
    // fields alone.
    ensure!(
        matches!(out.params.nodes[0], NodeConnection::Lnd(_))
            && matches!(out.params.nodes[1], NodeConnection::Cln(_))
            && matches!(out.params.nodes[2], NodeConnection::Eclair(_))
            && matches!(out.params.nodes[3], NodeConnection::LdkServer(_)),
        "node connection implementations were not inferred correctly from config"
    );

    let nodes = network.nodes();
    let pairs: Vec<_> = RING.iter().map(|(s, d)| (&nodes[*s], &nodes[*d])).collect();
    assert_activities_resolved(&out, &pairs);
    assert_defined_payments(&out, &pairs, count);

    // Each ring destination's own node reports the keysends it received. The final payment of
    // the run may still be settling (or its record lost to shutdown), hence the range.
    for (_, dest) in RING {
        let received = network.settled_keysend_count(dest).await? - receipts_before[dest];
        ensure!(
            (count - 1..=count).contains(&received),
            "node {} ({}) reports {received} received keysends, expected {} or {count}",
            dest,
            nodes[dest].alias,
            count - 1,
        );
    }
    Ok(())
}

/// A pair with no direct channel: the payment must route across the ring, proving route
/// construction from real graph data.
async fn scenario_multi_hop(network: &RealNetwork) -> anyhow::Result<()> {
    let count = 2u64;
    let scenario = Scenario::Defined {
        pairs: vec![(0, 2)],
        count: Some(count),
        style: ConfigStyle {
            amount_msat: ValueSpec::Scalar(5000),
            interval_secs: ValueSpec::Scalar(2),
            ..Default::default()
        },
    };
    let out = run(
        network,
        &scenario,
        RunOptions {
            total_time: Some(120),
            ..Default::default()
        },
    )
    .await?;

    let nodes = network.nodes();
    let pairs = [(&nodes[0], &nodes[2])];
    assert_activities_resolved(&out, &pairs);
    assert_defined_payments(&out, &pairs, count);
    Ok(())
}

/// Activities referencing nodes purely by alias, where the aliases come from the real nodes'
/// own configuration rather than a simulated graph description.
async fn scenario_alias_refs(network: &RealNetwork) -> anyhow::Result<()> {
    let count = 2u64;
    let scenario = Scenario::Defined {
        pairs: vec![(1, 3)],
        count: Some(count),
        style: ConfigStyle {
            source_ref: NodeRef::Alias,
            dest_ref: NodeRef::Alias,
            amount_msat: ValueSpec::Scalar(5000),
            interval_secs: ValueSpec::Scalar(2),
        },
    };
    let out = run(
        network,
        &scenario,
        RunOptions {
            total_time: Some(120),
            ..Default::default()
        },
    )
    .await?;

    let nodes = network.nodes();
    let pairs = [(&nodes[1], &nodes[3])];
    assert_activities_resolved(&out, &pairs);
    assert_defined_payments(&out, &pairs, count);
    Ok(())
}

/// Random activity across all four implementations, bounded by total time.
async fn scenario_random(network: &RealNetwork) -> anyhow::Result<()> {
    let out = run(
        network,
        &Scenario::Random { excludes: vec![] },
        RunOptions {
            total_time: Some(30),
            // Small expected amounts and a raised multiplier so a 30s window sees payments.
            expected_pmt_amt: 10_000,
            capacity_multiplier: 5.0,
            fix_seed: Some(42),
        },
    )
    .await?;

    assert_payments_dispatched(&out);
    assert_sources_controlled(&out, network.nodes());
    Ok(())
}
