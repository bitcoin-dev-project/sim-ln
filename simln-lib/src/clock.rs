use async_trait::async_trait;
use std::ops::{Div, Mul};
use std::time::{Duration, SystemTime};
use tokio::time::{self, Instant};

use crate::SimulationError;

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;

    async fn sleep(&self, wait: Duration);
}

/// Provides a wall clock implementation of the Clock trait.
#[derive(Clone)]
pub struct SystemClock {}

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    async fn sleep(&self, wait: Duration) {
        time::sleep(wait).await;
    }
}

/// Provides an implementation of the Clock trait that speeds up wall clock time by some factor.
#[derive(Clone)]
pub struct SimulationClock {
    // The multiplier
    speedup_multiplier: u32,

    /// Tracked so that we can calculate our "fast-forwarded" present relative to the time that we started running at.
    /// This is useful, because it allows us to still rely on the wall clock, then just convert based on our speedup.
    /// This field is expressed as a Duration for convenience.
    start_instant: Instant,
}

impl SimulationClock {
    /// Creates a new simulated clock that will speed up wall clock time by the multiplier provided, which must be in
    /// [1;1000] because our asynchronous sleep only supports a duration of ms granularity.
    pub fn new(speedup_multiplier: u32) -> Result<Self, SimulationError> {
        if speedup_multiplier < 1 {
            return Err(SimulationError::SimulatedNetworkError(
                "speedup_multiplier must be at least 1".to_string(),
            ));
        }

        if speedup_multiplier > 1000 {
            return Err(SimulationError::SimulatedNetworkError(
                "speedup_multiplier must be less than 1000, because the simulation sleeps with millisecond
                    granularity".to_string(),
            ));
        }

        Ok(SimulationClock {
            speedup_multiplier,
            start_instant: Instant::now(),
        })
    }

    /// Calculates the current simulation time based on the current wall clock time. Included for testing purposes
    /// so that we can fix the current wall clock time.
    fn calc_now(&self, now: SystemTime) -> SystemTime {
        now.checked_add(self.simulated_to_wall_clock(self.start_instant.elapsed()))
            .expect("simulation time overflow")
    }

    /// Converts a duration expressed in wall clock time to the amount of equivalent time that should be used in our
    /// sped up time.
    fn wall_clock_to_simulated(&self, d: Duration) -> Duration {
        d.div(self.speedup_multiplier)
    }

    /// Converts a duration expressed in sped up simulation time to the be expressed in wall clock time.
    fn simulated_to_wall_clock(&self, d: Duration) -> Duration {
        d.mul(self.speedup_multiplier)
    }
}

#[async_trait]
impl Clock for SimulationClock {
    /// To get the current time according to our simulation clock, we get the amount of wall clock time that has
    /// elapsed since the simulator clock was created and multiply it by our speedup.
    fn now(&self) -> SystemTime {
        self.calc_now(SystemTime::now())
    }

    /// To provide a sped up sleep time, we scale the proposed wait time by our multiplier and sleep.
    async fn sleep(&self, wait: Duration) {
        time::sleep(self.wall_clock_to_simulated(wait)).await;
    }
}
