//! PID-1 duties: reap orphaned children and wait for termination signals.
//! Only meaningful on Linux; guarded so the crate still builds elsewhere.

#[cfg(target_os = "linux")]
pub use linux::{spawn_reaper, wait_for_termination};

#[cfg(target_os = "linux")]
mod linux {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;
    use tokio::signal::unix::{signal, SignalKind};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, info};

    /// Continuously reap any orphaned children that get reparented to PID 1.
    /// Runs until `shutdown` is cancelled.
    ///
    /// Division of labour: this reaper handles *orphans* (processes reparented
    /// to PID 1 that nothing else waits on). Supervised service children are
    /// reaped by their own `run()`'s `wait()`, and killed on shutdown via the
    /// runner's `kill_on_drop`. There is a benign race if a child exits before
    /// this handler is installed — `reap_all` drains every reapable PID on the
    /// next SIGCHLD, so such a child is simply reaped slightly later.
    pub fn spawn_reaper(shutdown: CancellationToken) {
        tokio::spawn(async move {
            // SIGCHLD wakes us; we then reap everything reapable.
            let mut sigchld = match signal(SignalKind::child()) {
                Ok(s) => s,
                Err(e) => {
                    info!("could not install SIGCHLD handler (not PID 1?): {e}");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sigchld.recv() => {
                        reap_all();
                    }
                }
            }
        });
    }

    fn reap_all() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(status) => debug!(?status, "reaped child"),
            }
        }
    }

    /// Resolve when SIGTERM or SIGINT is received.
    pub async fn wait_for_termination() {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT");
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM"),
            _ = int.recv() => info!("received SIGINT"),
        }
    }
}

// Non-Linux stubs so `cargo build` works on dev machines/CI macos.
#[cfg(not(target_os = "linux"))]
pub fn spawn_reaper(_shutdown: tokio_util::sync::CancellationToken) {}

#[cfg(not(target_os = "linux"))]
pub async fn wait_for_termination() {
    let _ = tokio::signal::ctrl_c().await;
}
