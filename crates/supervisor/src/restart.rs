//! Restart policy: a pure decision, applied by `run_supervised`.

use crate::runner::RunOutcome;
use std::time::Duration;

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

/// Exponential restart backoff. Given the current backoff and how long the
/// service ran before exiting, return `(delay_to_sleep_now, next_backoff)`.
/// A service that stayed up at least `HEALTHY_RESET` is treated as recovered,
/// so its next restart waits the base delay rather than the escalated one —
/// long-lived services recover fast, while a binary that exits instantly keeps
/// escalating toward the cap instead of hot-looping.
pub fn backoff_step(current: Duration, ran_for: Duration) -> (Duration, Duration) {
    const BASE: Duration = Duration::from_secs(1);
    const CAP: Duration = Duration::from_secs(30);
    const HEALTHY_RESET: Duration = Duration::from_secs(60);
    let delay = if ran_for >= HEALTHY_RESET {
        BASE
    } else {
        current
    };
    let next = (delay * 2).min(CAP);
    (delay, next)
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

#[cfg(test)]
mod backoff_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn first_failure_uses_base_then_doubles() {
        let (delay, next) = backoff_step(Duration::from_secs(1), Duration::from_secs(0));
        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(next, Duration::from_secs(2));
    }

    #[test]
    fn doubles_up_to_cap() {
        assert_eq!(
            backoff_step(Duration::from_secs(2), Duration::ZERO).1,
            Duration::from_secs(4)
        );
        // 16s doubles to the 30s cap, not 32s.
        assert_eq!(
            backoff_step(Duration::from_secs(16), Duration::ZERO).1,
            Duration::from_secs(30)
        );
        // cap holds.
        assert_eq!(
            backoff_step(Duration::from_secs(30), Duration::ZERO),
            (Duration::from_secs(30), Duration::from_secs(30))
        );
    }

    #[test]
    fn healthy_run_resets_to_base() {
        // Ran 61s (>= 60s threshold) before exiting → next restart waits base, not the escalated value.
        let (delay, next) = backoff_step(Duration::from_secs(30), Duration::from_secs(61));
        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(next, Duration::from_secs(2));
        // Exactly 60s is on the boundary (>=) and resets too.
        assert_eq!(
            backoff_step(Duration::from_secs(30), Duration::from_secs(60)).0,
            Duration::from_secs(1)
        );
    }

    #[test]
    fn brief_run_keeps_escalating() {
        // Ran <60s → no reset; the current escalated delay is used.
        let (delay, _next) = backoff_step(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(delay, Duration::from_secs(8));
    }
}
