use crate::{NetworkGenerator, NodeInfo, PaymentGenerationError, PaymentGenerator};
use core::fmt;
use std::fmt::Display;
use tokio::time::Duration;

#[derive(Clone)]
pub struct DefinedActivityGenerator {
    destination: NodeInfo,
    wait: Duration,
    amount: u64,
}

impl DefinedActivityGenerator {
    pub fn new(destination: NodeInfo, wait: Duration, amount: u64) -> Self {
        DefinedActivityGenerator {
            destination,
            wait,
            amount,
        }
    }
}

impl Display for DefinedActivityGenerator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "static payment of {} to {} every {:?}",
            self.amount, self.destination, self.wait
        )
    }
}

impl NetworkGenerator for DefinedActivityGenerator {
    fn sample_node_by_capacity(
        &self,
        _source: bitcoin::secp256k1::PublicKey,
    ) -> (NodeInfo, Option<u64>) {
        (self.destination.clone(), None)
    }
}

impl PaymentGenerator for DefinedActivityGenerator {
    fn next_payment_wait(&self) -> Duration {
        self.wait
    }

    fn payment_amount(
        &self,
        destination_capacity: Option<u64>,
    ) -> Result<u64, crate::PaymentGenerationError> {
        match destination_capacity {
            Some(_) => Err(PaymentGenerationError(
                "destination amount must not be set for defined activity generator".to_string(),
            )),
            None => Ok(self.amount),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DefinedActivityGenerator;
    use crate::test_utils::{create_nodes, get_random_keypair};
    use crate::{NetworkGenerator, PaymentGenerationError, PaymentGenerator};
    use std::time::Duration;

    #[test]
    fn test_defined_activity_generator() {
        let node = create_nodes(1, 100000);
        let node = &node.first().unwrap().0;

        let source = get_random_keypair();
        let payment_amt = 50;

        let generator =
            DefinedActivityGenerator::new(node.clone(), Duration::from_secs(60), payment_amt);

        let (dest, dest_capacity) = generator.sample_node_by_capacity(source.1);
        assert_eq!(node.pubkey, dest.pubkey);
        assert!(dest_capacity.is_none());

        assert_eq!(payment_amt, generator.payment_amount(None).unwrap());
        assert!(matches!(
            generator.payment_amount(Some(10)),
            Err(PaymentGenerationError(..))
        ));
    }
}
