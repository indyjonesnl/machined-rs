//! Restart policy: a pure decision, applied by `run_supervised`.

use crate::runner::RunOutcome;

/// Restart behaviour for a supervised service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Never,
    OnFailure,
    Always,
}

/// Should the service run again after `outcome` under `policy`?
pub fn should_restart(policy: Policy, outcome: RunOutcome) -> bool {
    match policy {
        Policy::Never => false,
        Policy::OnFailure => outcome == RunOutcome::Failure,
        Policy::Always => outcome != RunOutcome::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_table() {
        use RunOutcome::*;
        assert!(!should_restart(Policy::Never, Failure));
        assert!(should_restart(Policy::OnFailure, Failure));
        assert!(!should_restart(Policy::OnFailure, Success));
        assert!(should_restart(Policy::Always, Success));
        assert!(should_restart(Policy::Always, Failure));
        assert!(!should_restart(Policy::Always, Stopped));
    }
}
