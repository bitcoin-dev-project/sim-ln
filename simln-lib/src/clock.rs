use async_trait::async_trait;
use std::time::{Duration, SystemTime};
use tokio::time::{self, Instant};

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
    async fn sleep(&self, wait: Duration);
}

/// Provides a flexible implementation of [Clock] that can be used for simulations.
///
/// For regular runtimes, this will just function as a regular wall clock, seeded with the provided starting time.
///
/// If run with a paused Tokio runtime, this clock provides the ability to speed up simulations. When no task is
/// runnable, the runtime will auto-advance to the next time instantly, providing the ability to "progress time"
/// rather than wait for these sleeps to elapse.
///
/// Some considerations for use of this mode:
/// * You cannot use SystemTime::now, as it will not match the simulation's
/// * If there are no tasks waiting, the simulation will "jump forward" to the next timer. Code interacting with the
///   simulator should be aware of this; if you're not actively interacting with it, it'll move on without you. There
///   is a "grace period" in the runtime that allows tasks the change to schedule themselves after the clock has been
///   progressed.
/// * This mode will misbehave if the code waits on operations that are not contained within its runtime. For example,
///   if waiting on I/O or a file operation, the tokio runtime can't see this wait and will advance without waiting on
///   the operation.
/// * If the system never reaches the state where all tasks are waiting, the clock will not advance. This may happen if
///   there is an always-pollable loop, for example.
///
/// The clock must be constructed on the runtime it will be driven on, because it captures a [tokio::time::Instant] whose
/// progression is governed by that runtime's (possibly paused) clock.
#[derive(Clone)]
pub struct SimulationClock {
    /// The wall clock starting time that this clock was created at, used to provide calendar timestamps.
    start_time: SystemTime,
    /// Tracks time elapsed since the clock was created, according to the runtime used.
    ///
    /// Note that in a paused runtime, this will capture the progression of "virtual time" as we jump forward.
    start_instant: Instant,
}

impl SimulationClock {
    /// Creates a new simulation clock that reports time relative to `start_time`.
    ///
    /// Must be called on the runtime that will drive the simulation.
    pub fn new(start_time: SystemTime) -> Self {
        SimulationClock {
            start_time,
            start_instant: Instant::now(),
        }
    }
}

#[async_trait]
impl Clock for SimulationClock {
    /// Reports the current time of the simulation.
    fn now(&self) -> SystemTime {
        self.start_time
            .checked_add(self.start_instant.elapsed())
            .expect("simulation clock time overflow")
    }

    /// Sleeps for the duration provided.
    ///
    /// If running on a paused runtime, the clock will skip forward to the earliest waiting sleep's time if no other
    /// tasks are runnable.
    async fn sleep(&self, wait: Duration) {
        time::sleep(wait).await;
    }
}

// The paused-runtime test relies on tokio's test-util, which is only enabled by the virtual-time feature.
#[cfg(all(test, feature = "virtual-time"))]
mod tests {
    use std::time::{Duration, SystemTime};

    use crate::clock::{Clock, SimulationClock};

    /// Tests that, on a paused runtime, the simulation clock advances by exactly the slept duration and consumes no
    /// wall-clock time.
    #[tokio::test(start_paused = true)]
    async fn test_simulation_clock_advances_on_sleep() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let clock = SimulationClock::new(start);

        // No virtual time has elapsed yet.
        assert_eq!(clock.now(), start);

        // Sleeping advances virtual time by exactly the requested duration.
        clock.sleep(Duration::from_secs(3600)).await;
        assert_eq!(clock.now(), start + Duration::from_secs(3600));

        // A subsequent sleep continues from where the previous one left off.
        clock.sleep(Duration::from_secs(60)).await;
        assert_eq!(clock.now(), start + Duration::from_secs(3660));
    }
}
