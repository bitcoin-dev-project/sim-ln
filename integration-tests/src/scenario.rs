//! The scenario layer: describes payment activity and the style in which it is written to config.
//! Scenarios reference nodes by index into a [`NodeHandle`] slice, so they are independent of how
//! the underlying network is provisioned.

use serde_json::{json, Value};

use crate::env::NodeHandle;

/// How a node is referenced in the config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRef {
    Alias,
    Pubkey,
}

impl NodeRef {
    fn to_json(self, node: &NodeHandle) -> Value {
        match self {
            NodeRef::Alias => json!(node.alias),
            NodeRef::Pubkey => json!(node.pubkey.to_string()),
        }
    }
}

/// A config value that is either a scalar or a `[min, max]` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSpec {
    Scalar(u64),
    Range(u64, u64),
}

impl ValueSpec {
    pub fn to_json(self) -> Value {
        match self {
            ValueSpec::Scalar(v) => json!(v),
            ValueSpec::Range(min, max) => json!([min, max]),
        }
    }

    /// Whether a concrete value could have been produced from this spec.
    pub fn contains(self, value: u64) -> bool {
        match self {
            ValueSpec::Scalar(v) => value == v,
            ValueSpec::Range(min, max) => (min..max).contains(&value),
        }
    }
}

/// The style in which defined activity is written to config: which identifier type references
/// nodes, and whether amounts/intervals are scalars or ranges.
#[derive(Debug, Clone, Copy)]
pub struct ConfigStyle {
    pub source_ref: NodeRef,
    pub dest_ref: NodeRef,
    pub amount_msat: ValueSpec,
    pub interval_secs: ValueSpec,
}

impl Default for ConfigStyle {
    fn default() -> Self {
        ConfigStyle {
            source_ref: NodeRef::Pubkey,
            dest_ref: NodeRef::Pubkey,
            amount_msat: ValueSpec::Scalar(1000),
            interval_secs: ValueSpec::Scalar(2),
        }
    }
}

/// Payment activity to run on a network.
#[derive(Debug, Clone)]
pub enum Scenario {
    /// Defined activity between pairs of nodes (indices into the handle slice), each dispatching
    /// `count` payments (or running until the simulation's total time when `None`).
    Defined {
        pairs: Vec<(usize, usize)>,
        count: Option<u64>,
        style: ConfigStyle,
    },
    /// Random activity across the network, excluding the given nodes (indices into the handle
    /// slice) from sending and receiving.
    Random { excludes: Vec<usize> },
}

impl Scenario {
    /// The `activity` section of the config; empty for random activity.
    pub fn activity_json(&self, nodes: &[NodeHandle]) -> Vec<Value> {
        match self {
            Scenario::Defined {
                pairs,
                count,
                style,
            } => pairs
                .iter()
                .map(|(source, dest)| {
                    let mut activity = json!({
                        "source": style.source_ref.to_json(&nodes[*source]),
                        "destination": style.dest_ref.to_json(&nodes[*dest]),
                        "interval_secs": style.interval_secs.to_json(),
                        "amount_msat": style.amount_msat.to_json(),
                    });
                    if let Some(count) = count {
                        activity["count"] = json!(count);
                    }
                    activity
                })
                .collect(),
            Scenario::Random { .. } => vec![],
        }
    }

    /// The `exclude` section of the config; empty for defined activity.
    pub fn exclude_json(&self, nodes: &[NodeHandle]) -> Vec<Value> {
        match self {
            Scenario::Defined { .. } => vec![],
            Scenario::Random { excludes } => excludes
                .iter()
                .map(|i| json!(nodes[*i].pubkey.to_string()))
                .collect(),
        }
    }
}
