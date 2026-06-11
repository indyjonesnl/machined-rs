//! Pure SNTP packet build + offset computation. No I/O.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{TimeError, TimeOffset};

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: i128 = 2_208_988_800;

/// Build a 48-byte SNTP client request: LI=0, VN=4, Mode=3 (`0x23`), rest zero.
pub fn build_request() -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = 0x23;
    p
}

/// Convert an 8-byte NTP timestamp (32.32 fixed point, 1900 epoch) to
/// nanoseconds since the Unix epoch.
fn ntp_to_unix_ns(b: &[u8]) -> i128 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i128;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as i128;
    let frac_ns = (frac * 1_000_000_000) >> 32;
    (secs - NTP_UNIX_OFFSET) * 1_000_000_000 + frac_ns
}

fn st_to_ns(t: SystemTime) -> i128 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Compute the clock offset from an SNTP response and the local send (T1) and
/// receive (T4) times: `offset = ((T2 - T1) + (T3 - T4)) / 2`.
/// Rejects a non-server response (mode != 4) or a zero transmit timestamp.
pub fn parse_offset(
    resp: &[u8; 48],
    t1: SystemTime,
    t4: SystemTime,
) -> Result<TimeOffset, TimeError> {
    if resp[0] & 0x07 != 4 {
        return Err(TimeError::BadResponse(
            "not a server (mode != 4) response".into(),
        ));
    }
    if resp[40..48].iter().all(|&x| x == 0) {
        return Err(TimeError::BadResponse("zero transmit timestamp".into()));
    }
    let t1n = st_to_ns(t1);
    let t4n = st_to_ns(t4);
    let t2n = ntp_to_unix_ns(&resp[32..40]);
    let t3n = ntp_to_unix_ns(&resp[40..48]);
    Ok(((t2n - t1n) + (t3n - t4n)) / 2)
}

/// Encode nanoseconds-since-Unix-epoch into an 8-byte NTP timestamp (test/server helper).
pub fn unix_ns_to_ntp(ns: i128) -> [u8; 8] {
    let secs = (ns.div_euclid(1_000_000_000) + NTP_UNIX_OFFSET) as u32;
    let frac = ((ns.rem_euclid(1_000_000_000) << 32) / 1_000_000_000) as u32;
    let mut b = [0u8; 8];
    b[0..4].copy_from_slice(&secs.to_be_bytes());
    b[4..8].copy_from_slice(&frac.to_be_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_well_formed() {
        let r = build_request();
        assert_eq!(r.len(), 48);
        assert_eq!(r[0], 0x23);
    }

    #[test]
    fn computes_offset() {
        // T1 = T4 = Unix epoch; server T2 = T3 = Unix 1000s → offset = +1000s.
        let mut resp = [0u8; 48];
        resp[0] = 0x24; // mode 4
        let ts = unix_ns_to_ntp(1000 * 1_000_000_000);
        resp[32..40].copy_from_slice(&ts);
        resp[40..48].copy_from_slice(&ts);
        let off = parse_offset(&resp, UNIX_EPOCH, UNIX_EPOCH).unwrap();
        assert_eq!(off, 1000 * 1_000_000_000);
    }

    #[test]
    fn rejects_non_server_and_zero_transmit() {
        let mut resp = [0u8; 48];
        resp[0] = 0x1b; // mode 3 (client) — not a server response
        let ts = unix_ns_to_ntp(0);
        resp[32..40].copy_from_slice(&ts);
        resp[40..48].copy_from_slice(&ts);
        assert!(parse_offset(&resp, UNIX_EPOCH, UNIX_EPOCH).is_err());

        let mut z = [0u8; 48];
        z[0] = 0x24;
        // transmit timestamp left zero → rejected
        assert!(parse_offset(&z, UNIX_EPOCH, UNIX_EPOCH).is_err());
    }
}
