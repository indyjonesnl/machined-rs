//! Real `TimeSync`: SNTP over UDP + `clock_settime`.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::sntp::{build_request, parse_offset};
use crate::{Result, TimeError, TimeOffset, TimeSync};

pub struct SntpTime;

impl SntpTime {
    pub fn new() -> Self {
        SntpTime
    }
}

impl Default for SntpTime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TimeSync for SntpTime {
    async fn query_offset(&self, addr: &str) -> Result<TimeOffset> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| TimeError::Io(e.to_string()))?;
        socket
            .connect(addr)
            .await
            .map_err(|e| TimeError::Io(e.to_string()))?;

        let req = build_request();
        let t1 = SystemTime::now();
        socket
            .send(&req)
            .await
            .map_err(|e| TimeError::Io(e.to_string()))?;

        let mut resp = [0u8; 48];
        let n = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut resp))
            .await
            .map_err(|_| TimeError::Timeout)?
            .map_err(|e| TimeError::Io(e.to_string()))?;
        let t4 = SystemTime::now();
        if n < 48 {
            return Err(TimeError::BadResponse("short response".into()));
        }
        parse_offset(&resp, t1, t4)
    }

    fn step_clock(&self, offset: TimeOffset) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use nix::sys::time::TimeSpec;
            use nix::time::{clock_gettime, clock_settime, ClockId};

            let now = clock_gettime(ClockId::CLOCK_REALTIME)
                .map_err(|e| TimeError::ClockSet(e.to_string()))?;
            let now_ns = now.tv_sec() as i128 * 1_000_000_000 + now.tv_nsec() as i128;
            let new_ns = now_ns + offset;
            let secs = new_ns.div_euclid(1_000_000_000) as i64;
            let nsecs = new_ns.rem_euclid(1_000_000_000) as i64;
            clock_settime(ClockId::CLOCK_REALTIME, TimeSpec::new(secs, nsecs))
                .map_err(|e| TimeError::ClockSet(e.to_string()))?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = offset;
            Err(TimeError::ClockSet(
                "clock_settime unsupported on this platform".into(),
            ))
        }
    }
}
