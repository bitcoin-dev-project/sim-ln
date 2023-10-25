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

Run with Simulation File (see [setup instructions](#simulation-file-setup) for details):
 ```
sim-cli sim.json
```

### Simulation File Setup
The simulator requires access details for a set of `nodes` that you 
have permission to execute commands on. Note that the current version 
of the simulator uses keysend to execute payments, which must be 
enabled in LND using `--accept-keysend`.

Payment activity can be simulated in two different ways:
* Random activity: generate random activity on the `nodes` provided, 
  using the graph topology to determine payment frequency and size.
* Defined activity: provide descriptions of specific payments that 
  you would like the generator to execute.

### Setup - Random Activity

To run the simulator with random activity generation, you just need to 
provide a set of nodes and the simulator will generate activity based 
on the topology of the underlying graph. Note that payments will only 
be sent between the `nodes` that are provided so that liquidity does 
not "drain" from the simulation.

```
{
  "nodes": [
    {
      "id": "Alice",
      "address": "https://127.0.0.1:10011",
      "macaroon": "/path/admin.macaroon",
      "cert": "/path/tls.cert"
    },
    { 
      "id": "0230a16a05c5ca120136b3a770a2adfdad88a68d526e63448a9eef88bddd6a30d8",
      "address": "https://localhost:10013",
      "ca_cert": "/path/ca.pem",
      "client_cert": "/path/client.pem",
      "client_key": "/path/client-key.pem"
    }
  ]
}
```

**Note that node addresses must be declare with HTTPS transport, i.e. <https://ip-or-domain>**

Nodes can be identified by an arbitrary string ("Alice", "CLN1", etc) or
by their node public key. If a valid public key is provided it *must* 
match the public key reported by the node.

There are a few cli flags that can be used to toggle the characteristics 
of the random activity that is generated: 
* `--expected-payment-amount`: the approximate average amount that 
  will be sent by nodes, randomness will be introduced such that larger
  nodes send a wider variety of payment sizes around this expectation.
* `--capacity-multiplier`: the number of times over that each node in 
  the network sends their capacity in a calendar month, for example:
  * `capacity-multiplier=2` means that each node sends double their 
     capacity in a month.
  * `capacity-multiplier=0.5` means that each node sends half their 
    capacity in a month.

### Setup - Defined Activity
If you would like SimLN to generate a specific payments between source 
and destination nodes, you can provide `activity` descriptions of the 
source, destination, frequency and amount for payments that you'd like 
to execute. Note that `source` nodes *must* be contained in `nodes`, 
but destination nodes can be any public node in the network (though 
this may result in liquidity draining over time).

The example simulation file below sets up the following simulation:
* Connect to `Alice` running LND to generate activity.
* Connect to `Bob` running CLN to generate activity.
* Dispatch 2000 msat payments from `Alice` to `Carol` every 1 seconds.
* Dispatch 140000 msat payments from `Bob` to `Alice` every 50 seconds.
* Dispatch 1000 msat payments from `Bob` to `Dave` every 2 seconds.
```
{
  "nodes": [
    {
      "id": "Alice",
      "address": "https://localhost:10011",
      "macaroon": "/path/admin.macaroon",
      "cert": "/path/tls.cert"
    },
    {
      "id": "0230a16a05c5ca120136b3a770a2adfdad88a68d526e63448a9eef88bddd6a30d8",
      "address": "https://127.0.0.1:10013",
      "ca_cert": "/path/ca.pem",
      "client_cert": "/path/client.pem",
      "client_key": "/path/client-key.pem"
    }
  ],
  "activity": [
    {
      "source": "Alice",
      "destination": "02d804ad31429c8cc29e20ec43b4129553eb97623801e534ab5a66cdcd2149dbed",
      "interval_secs": 1,
      "amount_msat": 2000
    },
    {
      "source": "0230a16a05c5ca120136b3a770a2adfdad88a68d526e63448a9eef88bddd6a30d8",
      "destination": "Alice",
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

**Note that node addresses must be declare with HTTPS transport, i.e <https://ip-or-domain>**

Nodes can be identified by their public key or an id string (as 
described above). Activity sources and destinations may reference the 
`id` defined in `nodes`, but destinations that are not listed in `nodes` 
*must* provide a valid public key.

### Simulation Output
A summary of the results will be logged by the simulator, and a full 
list of payments made with their outcomes is available in 
`simulation_{timestamp}.csv` in the directory that the simulation was 
executed in. For more detailed logs, use the `--log-level` cli flag.

## Lightning Environments
If you're looking to get started with local lightning development, we
recommend [polar](https://lightningpolar.com/). For larger deployments, 
see the [Scaling Lightning](https://github.com/scaling-lightning/scaling-lightning) 
project.

## Docker
If you want to run the cli in a containerized environment, see the docker set up docs [here](./docker/README.md)