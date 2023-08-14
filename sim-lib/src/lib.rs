use serde::{Deserialize, Serialize};

// Phase 0: User input - see config.json

// Phase 1: Parsed User Input

#[derive(Serialize, Deserialize, Debug)]
pub struct NodeConnection {
    id: String,
    host: String,
    macaroon: String,
    cert: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    nodes: Vec<NodeConnection>,
}

// Phase 2: Event Queue - TODO

// Phase 3: CSV output - TODO
