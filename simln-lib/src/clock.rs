use async_trait::async_trait;
use std::time::{Duration, SystemTime};
use tokio::time;

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
