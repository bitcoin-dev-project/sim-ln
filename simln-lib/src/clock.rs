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
    // The multiplier that the regular wall clock is sped up by, must be in [1, 1000].
    speedup_multiplier: u16,

    /// Tracked so that we can calculate our "fast-forwarded" present relative to the time that we started running at.
    /// This is useful, because it allows us to still rely on the wall clock, then just convert based on our speedup.
    /// This field is expressed as an Instant for convenience.
    start_instant: Instant,
}

impl SimulationClock {
    /// Creates a new simulated clock that will speed up wall clock time by the multiplier provided, which must be in
    /// [1;1000] because our asynchronous sleep only supports a duration of ms granularity.
    pub fn new(speedup_multiplier: u16) -> Result<Self, SimulationError> {
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

    /// Returns the instant that the simulation clock was started at.
    pub fn get_start_instant(&self) -> Instant {
        self.start_instant
    }

    /// Returns the speedup multiplier applied to time.
    pub fn get_speedup_multiplier(&self) -> u16 {
        self.speedup_multiplier
    }

    /// Calculates the current simulation time based on the current wall clock time.
    ///
    /// Separated for testing purposes so that we can fix the current wall clock time and elapsed interval.
    fn calc_now(&self, now: SystemTime, elapsed: Duration) -> SystemTime {
        now.checked_add(self.simulated_to_wall_clock(elapsed))
            .expect("simulation time overflow")
    }

    /// Converts a duration expressed in wall clock time to the amount of equivalent time that should be used in our
    /// sped up time.
    fn wall_clock_to_simulated(&self, d: Duration) -> Duration {
        d.div(self.speedup_multiplier.into())
    }

    /// Converts a duration expressed in sped up simulation time to the be expressed in wall clock time.
    fn simulated_to_wall_clock(&self, d: Duration) -> Duration {
        d.mul(self.speedup_multiplier.into())
    }
}

#[async_trait]
impl Clock for SimulationClock {
    /// To get the current time according to our simulation clock, we get the amount of wall clock time that has
    /// elapsed since the simulator clock was created and multiply it by our speedup.
    fn now(&self) -> SystemTime {
        self.calc_now(SystemTime::now(), self.start_instant.elapsed())
    }

    /// To provide a sped up sleep time, we scale the proposed wait time by our multiplier and sleep.
    async fn sleep(&self, wait: Duration) {
        time::sleep(self.wall_clock_to_simulated(wait)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use crate::clock::SimulationClock;

    /// Tests validation and that a multplier of 1 is a regular clock.
    #[test]
    fn test_simulation_clock() {
        assert!(SimulationClock::new(0).is_err());
        assert!(SimulationClock::new(1001).is_err());

        let clock = SimulationClock::new(1).unwrap();
        let now = SystemTime::now();
        let elapsed = Duration::from_secs(15);

        assert_eq!(
            clock.calc_now(now, elapsed),
            now.checked_add(elapsed).unwrap(),
        );
    }

    /// Test that time is sped up by multiplier.
    #[test]
    fn test_clock_speedup() {
        let clock = SimulationClock::new(10).unwrap();
        let now = SystemTime::now();

        assert_eq!(
            clock.calc_now(now, Duration::from_secs(1)),
            now.checked_add(Duration::from_secs(10)).unwrap(),
        );

        assert_eq!(
            clock.calc_now(now, Duration::from_secs(50)),
            now.checked_add(Duration::from_secs(500)).unwrap(),
        );
    }
}
