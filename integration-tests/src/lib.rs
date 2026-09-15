//! Integration test framework for sim-ln.
//!
//! The framework is split into independent layers:
//! - [`env`]: provisions a lightning network (simulated or real) and describes it as a partial
//!   simulation config, without any knowledge of the payments that will run on it.
//! - [`scenario`]: describes payment activity (defined or random) and the style in which it is
//!   written to config (aliases vs pubkeys, scalar vs range values), without any knowledge of how
//!   the underlying network is provisioned.
//! - [`runner`]: assembles a sim.json file from the two layers, runs it through the same public
//!   entry points the sim-cli binary uses, and collects observable output.
//! - [`asserts`]: shared assertions over that output.

pub mod asserts;
pub mod env;
pub mod retry;
pub mod runner;
pub mod scenario;
