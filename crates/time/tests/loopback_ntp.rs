//! Loopback fake-NTP-server integration (root-free) + a gated clock_settime test.

use std::time::{SystemTime, UNIX_EPOCH};

use machined_time::sntp::unix_ns_to_ntp;
use machined_time::{SntpTime, TimeSync};

#[tokio::test]
async fn queries_loopback_ntp_server() {
    // A fake NTP server: reply to one request with T2=T3 set to "now + 5s".
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap().to_string();

    let server_task = tokio::spawn(async move {
        let mut buf = [0u8; 48];
        let (_, peer) = server.recv_from(&mut buf).await.unwrap();
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let ts = unix_ns_to_ntp(now_ns + 5 * 1_000_000_000);
        let mut resp = [0u8; 48];
        resp[0] = 0x24; // mode 4
        resp[32..40].copy_from_slice(&ts);
        resp[40..48].copy_from_slice(&ts);
        server.send_to(&resp, peer).await.unwrap();
    });

    let offset = SntpTime::new().query_offset(&addr).await.unwrap();
    server_task.await.unwrap();

    // The server claimed ~+5s; allow generous slack for round-trip/scheduling.
    let secs = offset as f64 / 1e9;
    assert!(secs > 4.0 && secs < 6.0, "offset {secs}s not ~5s");
}

#[tokio::test]
#[ignore = "requires CAP_SYS_TIME (clock_settime)"]
async fn steps_clock() {
    let t = SntpTime::new();
    let before = SystemTime::now();
    t.step_clock(2 * 1_000_000_000).unwrap(); // +2s
    let after = SystemTime::now();
    let delta = after.duration_since(before).unwrap().as_secs_f64();
    // Restore.
    t.step_clock(-2 * 1_000_000_000).unwrap();
    assert!(delta > 1.5, "clock should have jumped ~2s, moved {delta}s");
}
