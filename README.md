# SimLN

SimLN is a simulation tool that can be used to generate realistic 
payment activity on any lightning network topology. It is intentionally 
environment-agnostic so that it can be used across many environments - 
from integration tests to public signets. 

This tool is intended to serve developers who are familiar with 
lightning network development. It may be useful to you if you are:
* A protocol developer looking to test proposals. 
* An application developer load-testing your application.
* A signet operator interested in a hands-off way to run an active node. 
* A researcher generating synthetic data for a target topology.

## Pre-Requisites
SimLN requires you to "bring your own network" to generate activity 
on. You will need:
* A [lightning network](#lightning-environments) connected with any 
  topology of channels.
* Access to execute commands on _at least_ one node in the network.
* Rust compiler [installed](https://www.rust-lang.org/tools/install).

## LN Implementation Support
* LND ✅ 
* CLN ✅ 
* Eclair 🏗️
* LDK-node 🏗️

See our [tracking issue](https://github.com/bitcoin-dev-project/sim-ln/issues/26)
for updates on implementation support (contributions welcome!).

## Configuration
The simulator is configured with two sets of information expressed in 
json: 
* `nodes`: a set of nodes that you have permission to execute 
  commands on.
* `activity`: a description of the payment activity that you would 
  like to generate.

The example config below sets up the following simulation:
* Connect to `Alice` running LND to generate activity.
* Connect to `Bob` running CLN to generate activity.
* Dispatch 2000 msat payments from `Alice` to `Carol` every 1 seconds.
* Dispatch 140000 msat payments from `Bob` to `Alice` every 50 seconds.
* Dispatch 1000 msat payments from `Bob` to `Dave` every 2 seconds.
```
{
  "nodes": [
    {
      "LND": {
        "id": "0257956239efc55dd6be91eff40c47749314ccf79cb15f79e30ca12f8622b6de9e",
        "address": "https://localhost:10011",
        "macaroon": "/path/admin.macaroon",
        "cert": "/path/tls.cert"
      }
    },
    {
      "CLN": {
        "id": "0230a16a05c5ca120136b3a770a2adfdad88a68d526e63448a9eef88bddd6a30d8",
        "address": "https://localhost:10013",
        "ca_cert": "/path/ca.pem",
        "client_cert": "/path/client.pem",
        "client_key": "/path/client-key.pem"
      }
    }
  ],
  "activity": [
    {
      "source": "0257956239efc55dd6be91eff40c47749314ccf79cb15f79e30ca12f8622b6de9e",
      "destination": "02d804ad31429c8cc29e20ec43b4129553eb97623801e534ab5a66cdcd2149dbed",
      "interval_secs": 1,
      "amount_msat": 2000
    },
    {
      "source": "0230a16a05c5ca120136b3a770a2adfdad88a68d526e63448a9eef88bddd6a30d8",
      "destination": "0257956239efc55dd6be91eff40c47749314ccf79cb15f79e30ca12f8622b6de9e",
      "interval_secs": 50,
      "amount_msat": 140000
    },
    {
      "source": "0230a16a05c5ca120136b3a770a2adfdad88a68d526e63448a9eef88bddd6a30d8",
      "destination": "03232e245059a2e7f6e32d6c4bca97fc4cda935c553ea3693adb3265a19050c3bf",
      "interval_secs": 2,
      "amount_msat": 1000
    }
  ]
}
```

Note that you do not need to have execution permissions on the destination 
nodes for keysend payments. This allows execution in environments such as 
signets, where not every node is under your control.

## Getting Started
Clone the repo: 
```
git clone https://github.com/bitcoin-dev-project/sim-ln
cd sim-ln
```

Install the CLI: 
```
cargo install --locked --path sim-cli/
```

Run Simulation with Config: 
```
sim-cli --config config.json
```

A summary of the results will be logged by the simulator, and a full 
list of payments made with their outcomes is available in 
`simulation_{timestamp}.csv` in the directory that the simulation was 
executed in. For more detailed logs, use the `--log-level` cli flag.

## Lightning Environments
If you're looking to get started with local lightning development, we
recommend [polar](https://lightningpolar.com/). For larger deployments, 
see the [Scaling Lightning](https://github.com/scaling-lightning/scaling-lightning) 
project.
