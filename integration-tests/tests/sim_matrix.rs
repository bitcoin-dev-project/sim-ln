//! The simulated-network test matrix: config-style and payment-modality coverage that is
//! backend-independent, run on virtual time so the whole matrix completes in seconds without
//! docker or external processes.

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use rstest::rstest;
use serde_json::json;
use sim_cli::parsing::NodeConnection;

use integration_tests::asserts::{
    assert_activities_resolved, assert_all_success, assert_amounts_within, assert_defined_payments,
    assert_not_involved, assert_payments_dispatched, assert_sources_controlled,
    assert_total_payments,
};
use integration_tests::env::simulated::SimulatedNetwork;
use integration_tests::env::TestNetwork;
use integration_tests::runner::{parse_params, run_simulated, RunOptions, SimFile, SimOutput};
use integration_tests::scenario::{ConfigStyle, NodeRef, Scenario, ValueSpec};

/// Runs a scenario on a simulated network and returns its output, failing the test on any setup
/// or simulation error.
fn run(network: &SimulatedNetwork, scenario: &Scenario, opts: RunOptions) -> SimOutput {
    let dir = tempfile::tempdir().expect("create temp dir");
    let sim_file = SimFile::new(network.config_fragment())
        .scenario(scenario, network.nodes())
        .write(dir.path())
        .expect("write sim file");
    run_simulated(&sim_file, opts).expect("simulation should run to completion")
}

/// Runs a scenario expected to fail validation and returns the full error text.
fn run_expecting_error(network: &SimulatedNetwork, activity: Vec<serde_json::Value>) -> String {
    let dir = tempfile::tempdir().expect("create temp dir");
    let sim_file = SimFile::new(network.config_fragment())
        .activity_raw(activity)
        .write(dir.path())
        .expect("write sim file");
    let err = run_simulated(&sim_file, RunOptions::default())
        .expect_err("simulation setup should fail validation");
    format!("{err:#}")
}

/// Defined activity across every combination of node reference style and value shape: however the
/// config spells out nodes and values, the same payments must flow. Includes a single-hop pair
/// (node 0 -> 1) and a multi-hop pair (spur node 4 -> 2, which must route through the ring).
#[rstest]
#[ntest::timeout(120_000)]
fn defined_activity_config_styles(
    #[values(NodeRef::Alias, NodeRef::Pubkey)] source_ref: NodeRef,
    #[values(NodeRef::Alias, NodeRef::Pubkey)] dest_ref: NodeRef,
    #[values(ValueSpec::Scalar(1000), ValueSpec::Range(1000, 10_000))] amount_msat: ValueSpec,
    #[values(ValueSpec::Scalar(2), ValueSpec::Range(1, 5))] interval_secs: ValueSpec,
) {
    let network = SimulatedNetwork::new();
    let count = 5;
    let scenario = Scenario::Defined {
        pairs: vec![(0, 1), (4, 2)],
        count: Some(count),
        style: ConfigStyle {
            source_ref,
            dest_ref,
            amount_msat,
            interval_secs,
        },
    };

    let out = run(&network, &scenario, RunOptions::default());

    let nodes = network.nodes();
    let pairs = [(&nodes[0], &nodes[1]), (&nodes[4], &nodes[2])];
    assert_activities_resolved(&out, &pairs);
    assert_amounts_within(&out, amount_msat);
    assert_defined_payments(&out, &pairs, count);
}

/// Defined activity without a count runs until the simulation's total time; on virtual time the
/// payment schedule is deterministic, so the exact number of dispatched payments is known.
#[rstest]
#[ntest::timeout(120_000)]
fn defined_activity_bounded_by_total_time() {
    let network = SimulatedNetwork::new();
    let scenario = Scenario::Defined {
        pairs: vec![(0, 1)],
        count: None,
        style: ConfigStyle {
            interval_secs: ValueSpec::Scalar(10),
            ..Default::default()
        },
    };

    let out = run(
        &network,
        &scenario,
        RunOptions {
            total_time: Some(95),
            ..Default::default()
        },
    );

    // Payments dispatch every 10 virtual seconds until shutdown at t=95: t=10..=90.
    assert_total_payments(&out, 9);
    assert_all_success(&out);
}

/// Random activity, with and without exclusions: payments flow between controlled nodes only,
/// and excluded nodes are never involved.
#[rstest]
#[case::no_exclusions(vec![])]
#[case::spur_node_excluded(vec![4])]
#[ntest::timeout(120_000)]
fn random_activity(#[case] excludes: Vec<usize>) {
    let network = SimulatedNetwork::new();
    let scenario = Scenario::Random {
        excludes: excludes.clone(),
    };

    let out = run(
        &network,
        &scenario,
        RunOptions {
            // A virtual day of activity with payments small relative to channel capacity.
            total_time: Some(86_400),
            expected_pmt_amt: 1_000_000,
            ..Default::default()
        },
    );

    assert_payments_dispatched(&out);
    assert_sources_controlled(&out, network.nodes());

    let excluded: Vec<_> = excludes.iter().map(|i| &network.nodes()[*i]).collect();
    assert_not_involved(&out, &excluded);
}

/// A seeded random run is reproducible: the same seed dispatches the identical payment sequence.
#[rstest]
#[ntest::timeout(120_000)]
fn random_activity_deterministic_with_seed() {
    let opts = RunOptions {
        total_time: Some(86_400),
        expected_pmt_amt: 1_000_000,
        fix_seed: Some(7),
        ..Default::default()
    };

    let run_once = || {
        let network = SimulatedNetwork::new();
        let out = run(&network, &Scenario::Random { excludes: vec![] }, opts);
        out.records
            .iter()
            .map(|r| (r.source, r.destination, r.amount_msat))
            .collect::<Vec<_>>()
    };

    let first = run_once();
    assert!(!first.is_empty());
    assert_eq!(
        first,
        run_once(),
        "seeded runs should dispatch identical payments"
    );
}

/// An activity source that is not a controlled node fails validation with a specific error.
#[rstest]
#[ntest::timeout(120_000)]
fn unknown_source_rejected() {
    let network = SimulatedNetwork::new();
    let error = run_expecting_error(
        &network,
        vec![json!({
            "source": "ghost",
            "destination": network.nodes()[1].alias,
            "interval_secs": 2,
            "amount_msat": 1000,
        })],
    );
    assert!(
        error.contains("not found in nodes"),
        "expected unknown-source validation error, got: {error}"
    );
}

/// An activity destination that exists nowhere in the graph fails validation.
#[rstest]
#[ntest::timeout(120_000)]
fn unknown_destination_rejected() {
    let network = SimulatedNetwork::new();
    let secp = Secp256k1::new();
    let stranger = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[99u8; 32]).unwrap());

    let error = run_expecting_error(
        &network,
        vec![json!({
            "source": network.nodes()[0].alias,
            "destination": stranger.to_string(),
            "interval_secs": 2,
            "amount_msat": 1000,
        })],
    );
    assert!(
        error.contains("unknown activity destination"),
        "expected unknown-destination validation error, got: {error}"
    );
}

/// A network where two nodes share an alias is rejected, since aliases would be ambiguous
/// references.
#[rstest]
#[ntest::timeout(120_000)]
fn duplicate_alias_rejected() {
    let network = SimulatedNetwork::with_duplicate_alias();
    let nodes = network.nodes();
    let error = run_expecting_error(
        &network,
        vec![json!({
            "source": nodes[0].pubkey.to_string(),
            "destination": nodes[1].pubkey.to_string(),
            "interval_secs": 2,
            "amount_msat": 1000,
        })],
    );
    assert!(
        error.contains("duplicated alias"),
        "expected duplicate-alias validation error, got: {error}"
    );
}

/// The untagged `nodes` section infers the right connector implementation purely from which
/// fields each entry carries. Parsing only; no connections are attempted.
#[rstest]
#[ntest::timeout(120_000)]
fn node_connection_implementation_inference() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("sim.json");
    std::fs::write(
        &path,
        json!({
            "nodes": [
                {
                    "id": "lnd-node",
                    "address": "localhost:10009",
                    "macaroon": "/tmp/simln-test/admin.macaroon",
                    "cert": "/tmp/simln-test/tls.cert",
                },
                {
                    "id": "cln-node",
                    "address": "localhost:9736",
                    "ca_cert": "/tmp/simln-test/ca.pem",
                    "client_cert": "/tmp/simln-test/client.pem",
                    "client_key": "/tmp/simln-test/client-key.pem",
                },
                {
                    "id": "eclair-node",
                    "base_url": "127.0.0.1:8080",
                    "api_username": "",
                    "api_password": "eclair-password",
                },
                {
                    "address": "https://127.0.0.1:3000",
                    "api_key": "6d6b3f5e",
                    "cert": "/tmp/simln-test/ldk-tls.crt",
                },
            ],
        })
        .to_string(),
    )
    .expect("write sim file");

    let params = parse_params(&path).expect("nodes section should deserialize");
    assert_eq!(params.nodes.len(), 4);
    assert!(matches!(params.nodes[0], NodeConnection::Lnd(_)));
    assert!(matches!(params.nodes[1], NodeConnection::Cln(_)));
    assert!(matches!(params.nodes[2], NodeConnection::Eclair(_)));
    assert!(matches!(params.nodes[3], NodeConnection::LdkServer(_)));
}
