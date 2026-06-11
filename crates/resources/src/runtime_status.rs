//! Container-runtime health resource. Pure data.

/// Observed container-runtime (CRI) health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub ready: bool,
    pub name: String,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let r = RuntimeStatus {
            ready: true,
            name: "containerd".into(),
            version: "2.0.0".into(),
        };
        assert!(r.ready);
    }
}
