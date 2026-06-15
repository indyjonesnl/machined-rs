//! Observed progress of an in-flight OS upgrade. Pure data.

/// Phase of an upgrade machined is performing (or last attempted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradePhase {
    Downloading,
    Verifying,
    Loaded,
    Failed,
}

/// Observed state of the current/last upgrade attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeStatus {
    pub phase: UpgradePhase,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let u = UpgradeStatus {
            phase: UpgradePhase::Failed,
            message: "sha mismatch".into(),
        };
        assert_eq!(u.phase, UpgradePhase::Failed);
    }
}
