# sim-ln

A Lightning Network simulation library that enables testing and analysis of payment routing behavior on any Lightning Network. While primarily designed for controlled environments, it can also be used with real networks like Bitcoin signet where you don't control all nodes.

## Overview

sim-ln provides a framework for simulating Lightning Network payment routing behavior, allowing developers and researchers to:

- Generate specific or random traffic patterns on a provided Lightning graph.
- Test payment routing strategies.
- Analyze network behavior under different conditions.

## Usage

Add sim-ln to your Cargo.toml:

```toml
[dependencies]
sim-ln = "0.1.0"
```