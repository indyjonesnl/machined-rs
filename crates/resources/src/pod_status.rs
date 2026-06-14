//! Observed state of a machined-run pod. Pure data.

/// Lifecycle phase of a pod machined runs via CRI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PodPhase {
    Pending,
    Running,
    Failed,
}

/// Observed state of one configured pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodStatus {
    pub name: String,
    pub phase: PodPhase,
    pub container_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_running() {
        let p = PodStatus {
            name: "hello".into(),
            phase: PodPhase::Running,
            container_id: "ctr-1".into(),
            message: String::new(),
        };
        assert_eq!(p.phase, PodPhase::Running);
    }
}
