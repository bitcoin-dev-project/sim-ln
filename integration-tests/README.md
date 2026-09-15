# Integration Tests

An integration test framework for sim-ln, split into two tiers:

- **Simulated tier** (`tests/sim_matrix.rs`): runs sim-ln against in-process simulated
  networks on virtual time. Covers the config-file surface (alias vs pubkey references,
  scalar vs range values, connector inference), payment modalities (defined and random)
  and negative validation cases. No external dependencies; completes in seconds.
- **Real-node tier** (`tests/real_nodes.rs`): spins up a heterogeneous regtest network in
  docker — bitcoind plus one node each of LND, CLN, Eclair and ldk-server, connected in a
  ring of channels — and runs simulations against it through each connector. Marked
  `#[ignore]` so it only runs when asked for explicitly.

## Running

```sh
make integration-sim    # simulated tier, no docker required
make integration-real   # real-node tier, requires a docker runtime
```

## Prerequisites for the real-node tier

- A running docker daemon. On macOS, Docker Desktop, colima and OrbStack all work; the
  test harness discovers the socket automatically via testcontainers.
- `protoc` (already required to build the workspace).
- Network access to pull the pinned node images on first run.

ldk-server has no published image, so on first use the harness runs `docker build`
against the upstream repository at the same rev the workspace's `ldk-server-client`
dependency pins, using upstream's own Dockerfile. The build takes a few minutes once and
is cached by docker thereafter. Set `SIMLN_LDK_SERVER_IMAGE=<image:tag>` to use a
prebuilt image instead — CI does this to reuse a cached build.

## Layout

- `src/env/` — the environment layer: provisions a network (simulated, or containers) and
  emits its part of the sim.json config. Knows nothing about payments.
- `src/scenario.rs` — the scenario layer: describes activity (defined/random) and config
  style. Knows nothing about how nodes are provisioned.
- `src/runner.rs` — assembles sim.json files, runs them through the same public entry
  points the sim-cli binary uses, and collects results.
- `src/asserts.rs` — shared assertions over simulation output.
- `src/retry.rs` — capped-exponential-backoff polling used throughout real-node startup.

The layering is the point: adding a node implementation touches only `src/env/containers/`,
and adding a payment scenario touches only the scenario layer and tests.
