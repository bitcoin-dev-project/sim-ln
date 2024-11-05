use crate::sim_node::{
    CriticalError, CustomRecords, ForwardingError, InterceptRequest, Interceptor,
};
use crate::SimulationError;
use async_trait::async_trait;
use rand_distr::{Distribution, Poisson};
use std::time::Duration;
use tokio::{select, time};

/// LatencyIntercepor is a HTLC interceptor that will delay HTLC forwarding by some randomly chosen delay.
pub struct LatencyIntercepor<D>
where
    D: Distribution<f32> + Send + Sync,
{
    latency_dist: D,
}

impl LatencyIntercepor<Poisson<f32>> {
    pub fn new_poisson(lambda_ms: f32) -> Result<Self, SimulationError> {
        let poisson_dist = Poisson::new(lambda_ms).map_err(|e| {
            SimulationError::SimulatedNetworkError(format!("Could not create possion: {e}"))
        })?;

        Ok(Self {
            latency_dist: poisson_dist,
        })
    }
}

#[async_trait]
impl<D> Interceptor for LatencyIntercepor<D>
where
    D: Distribution<f32> + Send + Sync,
{
    /// Introduces a random sleep time on the HTLC.
    async fn intercept_htlc(
        &self,
        req: InterceptRequest,
    ) -> Result<Result<CustomRecords, ForwardingError>, CriticalError> {
        let latency = self.latency_dist.sample(&mut rand::thread_rng());

        select! {
            _ = req.shutdown_listener => log::debug!("Latency interceptor exiting due to shutdown signal received."),
            _ = time::sleep(Duration::from_millis(latency as u64)) => {}
        }
        Ok(Ok(CustomRecords::default()))
    }

    fn name(&self) -> String {
        "Latency Interceptor".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Interceptor, LatencyIntercepor};
    use crate::sim_node::{CustomRecords, HtlcRef, InterceptRequest};
    use crate::test_utils::get_random_keypair;
    use crate::ShortChannelID;
    use lightning::ln::PaymentHash;
    use ntest::assert_true;
    use rand::distributions::Distribution;
    use rand::Rng;
    use tokio::time::timeout;
    use triggered::Trigger;

    /// Always returns the same value, useful for testing.
    struct ConstantDistribution {
        value: f32,
    }

    impl Distribution<f32> for ConstantDistribution {
        fn sample<R: Rng + ?Sized>(&self, _rng: &mut R) -> f32 {
            self.value
        }
    }

    fn test_request() -> (InterceptRequest, Trigger) {
        let (shutdown_trigger, shutdown_listener) = triggered::trigger();

        let (_, pk) = get_random_keypair();
        let request = InterceptRequest {
            forwarding_node: pk,
            payment_hash: PaymentHash([0; 32]),
            incoming_htlc: HtlcRef {
                channel_id: ShortChannelID::from(123),
                index: 1,
            },
            incoming_custom_records: CustomRecords::default(),
            outgoing_channel_id: None,
            incoming_amount_msat: 100,
            outgoing_amount_msat: 50,
            incoming_expiry_height: 120,
            outgoing_expiry_height: 100,
            shutdown_listener,
        };

        (request, shutdown_trigger)
    }

    /// Tests that the interceptor exits immediately if a shutdown signal is received.
    #[tokio::test]
    async fn test_shutdown_signal() {
        // Set fixed dist to a high value so that the test won't flake.
        let latency_dist = ConstantDistribution { value: 1000.0 };
        let interceptor = LatencyIntercepor { latency_dist };

        let (request, trigger) = test_request();
        trigger.trigger();

        assert_true!(timeout(Duration::from_secs(10), async {
            interceptor.intercept_htlc(request).await
        })
        .await
        .is_ok());
    }

    /// Tests the happy case where we wait for our latency and then return a result.
    #[tokio::test]
    async fn test_latency_response() {
        let latency_dist = ConstantDistribution { value: 0.0 };
        let interceptor = LatencyIntercepor { latency_dist };

        let (request, _) = test_request();
        // We should return immediately because timeout is zero.
        assert_true!(timeout(Duration::from_secs(1), async {
            interceptor.intercept_htlc(request).await
        })
        .await
        .is_ok());
    }
}
