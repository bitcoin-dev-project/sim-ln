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

To run the simulation, create a simulation file `sim.json` in the working directory (see [setup instructions](#simulation-file-setup) for details) and run:

```
sim-cli
```

Run `sim-cli -h` for details on `--data-dir` and `--sim-file` options that allow specifying custom simulation file locations.

Interested in contributing to the project? See [CONTRIBUTING](CONTRIBUTING.md) for more details.

### Simulation File Setup
The simulator requires access details for a set of `nodes` that you 
have permission to execute commands on. Note that the current version 
of the simulator uses keysend to execute payments, which must be 
enabled in LND using `--accept-keysend` (for CLN node it is enabled by default).

The required access details will depend on the node implementation. For LND, the following
information is required:

```
{
  "id": <node_id>,
  "address": https://<ip:port or domain:port>,
  "macaroon": <path_to_selected_macaroon>,
  "cert": <path_to_tls_cert>
}
```

Whereas for CLN nodes, the following information is required:

```
{ 
  "id": <node_id>,
  "address": https://<ip:port or domain:port>,
  "ca_cert": <path_to_ca_cert>,
  "client_cert": <path_to_client_cert>,
  "client_key": <path_to_client_key>
}
```

**Note that node addresses must be declare with HTTPS transport, i.e. <https://ip-or-domain:port>**

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

A summary of the results will be logged by the simulator. A full list of payments made with their outcomes
is available in `simulation_{timestamp}.csv` within the configured `{data_dir}/results`.

Run `sim-cli -h` for details on data directory (`--data-dir`) and other options including `--print-batch-size`
which affect how simulation outputs are persisted

## Lightning Environments
If you're looking to get started with local lightning development, we
recommend [polar](https://lightningpolar.com/). For larger deployments, 
see the [Scaling Lightning](https://github.com/scaling-lightning/scaling-lightning) 
project.

## Docker
If you want to run the cli in a containerized environment, see the docker set up docs [here](./docker/README.md)

## Advanced Usage - Network Simulation

If you are looking to simulate payments are large lightning networks 
without the resource consumption of setting up a large cluster of nodes,
you may be interested in dispatching payments on a simulated network.

To run on a simulated network, you will need to provide the desired 
topology and channel policies for each edge in the graph. The nodes in 
the network will be inferred from the edges provided. Simulation of 
payments on both a mocked and real lightning network is not supported, 
so the `sim_network` field is mutually exclusive with the `nodes` section 
described above.

The example that follows will execute random payments on a network 
consisting of three nodes and two channels. You may specify defined 
activity to execute on the mocked network, though you must refer to 
nodes by their pubkey (aliases are not yet supported).

```
{
  "sim_network": [
    {
      "scid": 1,
      "capacity_msat": 250000,
      "node_1": {
        "pubkey": "0344f37d544896dcc95a08ddd9bdfc2b756bf3f91b3f65bce588bd9d0228c24977",
        "max_htlc_count": 483,
        "max_in_flight_msat": 250000,
        "min_htlc_size_msat": 1,
        "max_htlc_size_msat": 100000,
        "cltv_expiry_delta": 40,
        "base_fee": 1000,
        "fee_rate_prop": 100
      },
      "node_2": {
        "pubkey": "020a30431ce58843eedf8051214dbfadb65b107cc598b8277f14bb9b33c9cd026f",
        "max_htlc_count": 15,
        "max_in_flight_msat": 100000,
        "min_htlc_size_msat": 1,
        "max_htlc_size_msat": 50000,
        "cltv_expiry_delta": 80,
        "base_fee": 2000,
        "fee_rate_prop": 500
      }
    },
    {
      "scid": 2,
      "capacity_msat": 100000,
      "node_1": {
        "pubkey": "020a30431ce58843eedf8051214dbfadb65b107cc598b8277f14bb9b33c9cd026f",
        "max_htlc_count": 200,
        "max_in_flight_msat": 100000,
        "min_htlc_size_msat": 1,
        "max_htlc_size_msat": 25000,
        "cltv_expiry_delta": 40,
        "base_fee": 1750,
        "fee_rate_prop": 100
      },
      "node_2": {
        "pubkey": "035c0b392725bb7298d56bf1bcb23634fc509d86a39a8141d435f9d4d6cd4b12eb",
        "max_htlc_count": 15,
        "max_in_flight_msat": 50000,
        "min_htlc_size_msat": 1,
        "max_htlc_size_msat": 50000,
        "cltv_expiry_delta": 80,
        "base_fee": 3000,
        "fee_rate_prop": 5
      }
    }
  ]
}
```

Note that you need to provide forwarding policies in each direction, 
because each participant in the channel sets their own forwarding 
policy and restrictions on their counterparty. 

### Inclusions and Limitations

The simulator will execute payments on the mocked out network as it 
would for a network of real nodes. See the inclusions and exclusions 
listed below for a description of the functionality covered by the 
simulated network.

Included: 
* Routing Policy Enforcement: mocked channels enforce fee and CLTV 
  requirements.
* Channel restrictions: mocked channels abide by the in-flight 
  count and value limitations set on channel creation.
* Liquidity checks: HTLCs are only forwarded if the node has sufficient
  liquidity in the mocked channel.

Not included: 
* Channel reserve: the required minimum reserve balance is not 
  subtracted from a node's available balance.
* On chain fees: the simulation does not subtract on chain fees from 
  available liquidity.
* Dust limits: the simulation node not account for restrictions on dust
  HTLCs.
