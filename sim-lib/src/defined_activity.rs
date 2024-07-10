use crate::{
    DestinationGenerationError, DestinationGenerator, NodeInfo, PaymentGenerationError,
    PaymentGenerator, RngSend, ValueOrRange,
};
use std::fmt;
use tokio::time::Duration;

#[derive(Clone)]
pub struct DefinedPaymentActivity {
    destination: NodeInfo,
    start: Option<Duration>,
    count: Option<u64>,
    wait: ValueOrRange<u16>,
    amount: ValueOrRange<u64>,
}

impl DefinedPaymentActivity {
    pub fn new(
        destination: NodeInfo,
        start: Option<Duration>,
        count: Option<u64>,
        wait: ValueOrRange<u16>,
        amount: ValueOrRange<u64>,
    ) -> Self {
        DefinedPaymentActivity {
            destination,
            start,
            count,
            wait,
            amount,
        }
    }
}

impl fmt::Display for DefinedPaymentActivity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "static payment of {} to {} every {}s",
            self.amount, self.destination, self.wait
        )
    }
}

impl DestinationGenerator for DefinedPaymentActivity {
    fn choose_destination(
        &self,
        _: &mut RngSend,
        _: bitcoin::secp256k1::PublicKey,
    ) -> Result<(NodeInfo, Option<u64>), DestinationGenerationError> {
        Ok((self.destination.clone(), None))
    }
}

impl PaymentGenerator for DefinedPaymentActivity {
    fn payment_start(&self) -> Option<Duration> {
        self.start
    }

    fn payment_count(&self) -> Option<u64> {
        self.count
    }

    fn next_payment_wait(&self, _: &mut RngSend) -> Result<Duration, PaymentGenerationError> {
        Ok(Duration::from_secs(self.wait.value() as u64))
    }

    fn payment_amount(
        &self,
        _: &mut RngSend,
        destination_capacity: Option<u64>,
    ) -> Result<u64, crate::PaymentGenerationError> {
        if destination_capacity.is_some() {
            Err(PaymentGenerationError(
                "destination amount must not be set for defined activity generator".to_string(),
            ))
        } else {
            Ok(self.amount.value())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DefinedPaymentActivity;
    use crate::test_utils::{create_nodes, get_random_keypair};
    use crate::{DestinationGenerator, PaymentGenerationError, PaymentGenerator, RngSend};

    #[test]
    fn test_defined_activity_generator() {
        let node = create_nodes(1, 100000);
        let node = &node.first().unwrap().0;

        let source = get_random_keypair();
        let payment_amt = 50;

        let generator = DefinedPaymentActivity::new(
            node.clone(),
            None,
            None,
            crate::ValueOrRange::Value(60),
            crate::ValueOrRange::Value(payment_amt),
        );

        let mut rng = RngSend::new(None);
        let (dest, dest_capacity) = generator.choose_destination(&mut rng, source.1).unwrap();
        assert_eq!(node.pubkey, dest.pubkey);
        assert!(dest_capacity.is_none());

        assert_eq!(
            payment_amt,
            generator.payment_amount(&mut rng, None).unwrap()
        );
        assert!(matches!(
            generator.payment_amount(&mut rng, Some(10)),
            Err(PaymentGenerationError(..))
        ));
    }
}
