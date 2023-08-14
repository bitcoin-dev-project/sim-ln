use serde::{Deserialize, Serialize};

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

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
