# sim-ln

A Lightning Network simulation library that enables testing and analysis of payment routing behavior in a controlled environment.

## Overview

sim-ln provides a framework for simulating Lightning Network payment routing behavior, allowing developers and researchers to:

- Create simulated Lightning Network topologies
- Test payment routing strategies
- Analyze network behavior under different conditions
- Simulate real Lightning node implementations (LND, c-lightning, Eclair)

## Usage

Add sim-ln to your Cargo.toml:

```toml
[dependencies]
sim-ln = "0.1.0"
```

### Basic Example

```rust
use sim_ln::{SimulationCfg, Simulation, NodeInfo, LightningNode};

// Create simulation configuration
let config = SimulationCfg::new(
    Some(3600),           // Run for 1 hour
    1_000_000,           // 1000 sat expected payment size
    1.0,                 // Normal activity level
    None,                // No result writing
    Some(42),            // Random seed for reproducibility
);

// Set up nodes and channels
// ... configure your network topology

// Create and run simulation
let simulation = Simulation::new(config, nodes, activity, task_tracker);
simulation.run().await?;
```
