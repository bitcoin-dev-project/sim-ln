//! Runtime helpers for driving a simulated network on virtual time.
//!
//! Running a simulation on virtual time requires a very specific runtime: single-threaded (so task scheduling is
//! deterministic) and time-paused (so virtual time auto-advances to the next timer instead of sleeping on the wall
//! clock). Rather than ask callers to configure this themselves and risk silently losing determinism or the virtual-time
//! advance, the library builds and owns the runtime here.

use std::future::Future;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::runtime::{Builder, Handle};

use crate::clock::SimulationClock;
use crate::SimulationError;

/// Runs a simulation to completion on virtual time and returns its result.
///
/// A single-threaded, time-paused Tokio runtime is created and owned for the duration of the call. On this runtime,
/// once every task is parked on a timer the runtime advances virtual time straight to the next timer, so the simulation
/// runs as fast as the CPU allows. A [SimulationClock] anchored at `start_time` is constructed on the runtime and handed
/// to `build`, which should build the network and simulation against that clock and drive them to completion.
///
/// This must not be called from within an existing Tokio runtime (for example, from an `async fn` or a `#[tokio::main]`
/// entry point): a runtime cannot be nested inside another. Doing so returns a [SimulationError::RuntimeError] rather
/// than panicking. Call it from a synchronous context instead.
///
/// # Example
///
/// ```no_run
/// use std::time::{Duration, SystemTime};
/// use simln_lib::clock::Clock;
/// use simln_lib::runtime::block_on_virtual_time;
///
/// let _now = block_on_virtual_time(SystemTime::UNIX_EPOCH, |clock| async move {
///     // Build your network and simulation against `clock`, then run it. A day-long sleep returns immediately.
///     clock.sleep(Duration::from_secs(86_400)).await;
///     Ok::<_, simln_lib::SimulationError>(clock.now())
/// })??;
/// # Ok::<(), simln_lib::SimulationError>(())
/// ```
pub fn block_on_virtual_time<F, Fut, T>(
    start_time: SystemTime,
    build: F,
) -> Result<T, SimulationError>
where
    F: FnOnce(Arc<SimulationClock>) -> Fut,
    Fut: Future<Output = T>,
{
    if Handle::try_current().is_ok() {
        return Err(SimulationError::RuntimeError(
            "block_on_virtual_time cannot be called from within a Tokio runtime; call it from a \
             synchronous context so it can own a paused single-threaded runtime"
                .to_string(),
        ));
    }

    let runtime = Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .map_err(|e| {
            SimulationError::RuntimeError(format!("could not build virtual-time runtime: {e}"))
        })?;

    Ok(runtime.block_on(async move {
        // Construct the clock on the runtime so that its virtual-time instant is governed by the paused runtime.
        let clock = Arc::new(SimulationClock::new(start_time));
        build(clock).await
    }))
}

#[cfg(test)]
mod tests {
    use super::block_on_virtual_time;
    use crate::clock::Clock;
    use std::time::{Duration, SystemTime};

    /// A simulated day elapses with no real wall-clock wait, and the clock reflects the advance.
    #[test]
    fn test_block_on_virtual_time_advances_virtually() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let now = block_on_virtual_time(start, |clock| async move {
            clock.sleep(Duration::from_secs(86_400)).await;
            clock.now()
        })
        .unwrap();

        assert_eq!(now, start + Duration::from_secs(86_400));
    }

    /// Calling from within a runtime is rejected rather than panicking.
    #[tokio::test]
    async fn test_block_on_virtual_time_rejects_nested_runtime() {
        let result = block_on_virtual_time(SystemTime::UNIX_EPOCH, |_clock| async {});
        assert!(result.is_err());
    }
}
