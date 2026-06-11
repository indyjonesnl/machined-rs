//! Time synchronisation: a `TimeSync` trait with a pure-Rust SNTP `SntpTime`
//! implementation and an in-memory fake.

pub mod fake;
pub mod real;
pub mod sntp;

use async_trait::async_trait;

pub use fake::FakeTimeSync;
pub use real::SntpTime;
pub use sntp::{build_request, parse_offset};

/// Clock offset (server minus local) in signed nanoseconds.
pub type TimeOffset = i128;

#[derive(thiserror::Error, Debug)]
pub enum TimeError {
    #[error("time io: {0}")]
    Io(String),
    #[error("time query timed out")]
    Timeout,
    #[error("bad ntp response: {0}")]
    BadResponse(String),
    #[error("clock set: {0}")]
    ClockSet(String),
}

pub type Result<T> = std::result::Result<T, TimeError>;

/// Query an NTP server for the clock offset and step the system clock.
#[async_trait]
pub trait TimeSync: Send + Sync {
    /// One SNTP round-trip against `addr` (`"host:port"`). Returns the offset.
    async fn query_offset(&self, addr: &str) -> Result<TimeOffset>;
    /// Step `CLOCK_REALTIME` by `offset`.
    fn step_clock(&self, offset: TimeOffset) -> Result<()>;
}
