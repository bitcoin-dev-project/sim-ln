use crate::{DestinationGenerator, NodeInfo, PaymentGenerationError, PaymentGenerator};
use std::fmt;
use tokio::time::Duration;

#[derive(Clone)]
pub struct DefinedPaymentActivity {
    destination: NodeInfo,
    wait: Duration,
    amount: u64,
}

impl DefinedPaymentActivity {
    pub fn new(destination: NodeInfo, wait: Duration, amount: u64) -> Self {
        DefinedPaymentActivity {
            destination,
            wait,
            amount,
        }
    }
}

impl fmt::Display for DefinedPaymentActivity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "static payment of {} to {} every {:?}",
            self.amount, self.destination, self.wait
        )
    }
}

impl DestinationGenerator for DefinedPaymentActivity {
    fn choose_destination(&self, _: bitcoin::secp256k1::PublicKey) -> (NodeInfo, Option<u64>) {
        (self.destination.clone(), None)
    }
}

impl PaymentGenerator for DefinedPaymentActivity {
    fn next_payment_wait(&self) -> Duration {
        self.wait
    }

    fn payment_amount(&self, destination_capacity: Option<u64>) -> Result<u64, crate::PaymentGenerationError> {
        if destination_capacity.is_some() {
            Err(PaymentGenerationError(
                "destination amount must not be set for defined activity generator".to_string(),
            ))
        } else {
            Ok(self.amount)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DefinedPaymentActivity;
    use crate::test_utils::{create_nodes, get_random_keypair};
    use crate::{DestinationGenerator, PaymentGenerationError, PaymentGenerator};
    use std::time::Duration;

    #[test]
    fn test_defined_activity_generator() {
        let node = create_nodes(1, 100000);
        let node = &node.first().unwrap().0;

        let source = get_random_keypair();
        let payment_amt = 50;

        let generator = DefinedPaymentActivity::new(node.clone(), Duration::from_secs(60), payment_amt);

        let (dest, dest_capacity) = generator.choose_destination(source.1);
        assert_eq!(node.pubkey, dest.pubkey);
        assert!(dest_capacity.is_none());

        assert_eq!(payment_amt, generator.payment_amount(None).unwrap());
        assert!(matches!(
            generator.payment_amount(Some(10)),
            Err(PaymentGenerationError(..))
        ));
    }
}
