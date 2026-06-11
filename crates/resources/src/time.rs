//! Time-sync resource (observed state). Pure data.

/// Observed clock-sync state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeStatus {
    pub synced: bool,
    pub server: String,
    pub offset_ns: i64,
    pub sync_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let t = TimeStatus {
            synced: true,
            server: "0.pool.ntp.org".into(),
            offset_ns: -1234,
            sync_count: 1,
        };
        assert!(t.synced);
    }
}
