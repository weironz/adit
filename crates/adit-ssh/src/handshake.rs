//! A network handshake budget which excludes time spent awaiting host-key trust.
use super::{effective_timeout_secs, SshError};
use std::{io, time::Duration};
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone)]
struct State {
    active: bool,
    paused: bool,
    deadline: Instant,
    remaining: Duration,
    failure: Option<TransportFailure>,
}

#[derive(Debug, Clone)]
struct TransportFailure {
    kind: io::ErrorKind,
    message: String,
    raw_os_error: Option<i32>,
}

impl TransportFailure {
    fn ssh_error(&self) -> SshError {
        let error = match self.raw_os_error {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::new(self.kind, self.message.clone()),
        };
        SshError::Russh(russh::Error::IO(error))
    }
}

#[derive(Debug, Clone)]
pub(super) struct HandshakeBudget {
    state: watch::Sender<State>,
    configured_secs: u64,
}

/// Owns the handshake. Dropping a connect future must also invalidate any
/// prompt held by russh's independently spawned key-exchange task.
pub(super) struct HandshakeGuard(HandshakeBudget);

pub(super) struct TrustWait(HandshakeBudget);

impl HandshakeBudget {
    pub(super) fn new(configured_secs: u64) -> (Self, HandshakeGuard) {
        let remaining = Duration::from_secs(effective_timeout_secs(configured_secs));
        let (state, _) = watch::channel(State {
            active: true,
            paused: false,
            deadline: Instant::now() + remaining,
            remaining,
            failure: None,
        });
        let budget = Self {
            state,
            configured_secs,
        };
        (budget.clone(), HandshakeGuard(budget))
    }

    pub(super) fn pause_for_trust(&self) -> Result<TrustWait, SshError> {
        let mut result = Err(SshError::AuthenticationCancelled);
        self.state.send_if_modified(|state| {
            if !state.active {
                return false;
            }
            let now = Instant::now();
            if now >= state.deadline {
                result = Err(SshError::Timeout(self.configured_secs));
                return false;
            }
            debug_assert!(!state.paused);
            state.remaining = state.deadline.duration_since(now);
            state.paused = true;
            result = Ok(TrustWait(self.clone()));
            true
        });
        result
    }

    /// Run a synchronous reply operation while cancellation cannot overtake it.
    pub(super) fn while_active(&self, operation: impl FnOnce() -> bool) -> bool {
        let state = self.state.borrow();
        state.active && operation()
    }

    /// Linearize a received decision against cancellation. Once this wins, its
    /// write may finish after a later cancellation. Never hold the watch lock
    /// across disk I/O: the UI also reads it to check whether a prompt is live.
    pub(super) fn claim_decision(&self) -> bool {
        self.state.borrow().active
    }

    /// Wake the connect owner as well as invalidating its outstanding prompt.
    /// Russh may be stuck shutting down a pipe in its detached kex task; the
    /// owner must be able to drop its ProxyCommand process without waiting.
    pub(super) fn fail_transport(&self, error: &io::Error) {
        self.state.send_if_modified(|state| {
            if !state.active || !state.paused {
                return false;
            }
            state.active = false;
            state.failure = Some(TransportFailure {
                kind: error.kind(),
                message: error.to_string(),
                raw_os_error: error.raw_os_error(),
            });
            true
        });
    }

    pub(super) async fn failed(&self) -> SshError {
        let mut changes = self.state.subscribe();
        loop {
            let state = changes.borrow_and_update().clone();
            if let Some(failure) = state.failure {
                return failure.ssh_error();
            }
            if !state.active || changes.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    pub(super) async fn cancelled(&self) {
        let mut changes = self.state.subscribe();
        loop {
            if !changes.borrow_and_update().active {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) async fn expired(&self) {
        let mut changes = self.state.subscribe();
        loop {
            let state = changes.borrow_and_update().clone();
            if !state.active {
                return std::future::pending().await;
            }
            if state.paused {
                if changes.changed().await.is_err() {
                    return;
                }
                continue;
            }
            tokio::select! {
                biased;
                _ = changes.changed() => {}
                _ = tokio::time::sleep_until(state.deadline) => {
                    // A trust wait may have started while the timer was waking.
                    let current = changes.borrow_and_update().clone();
                    if current.active && !current.paused && Instant::now() >= current.deadline {
                        return;
                    }
                }
            }
        }
    }
}

impl Drop for TrustWait {
    fn drop(&mut self) {
        self.0.state.send_if_modified(|state| {
            if !state.active || !state.paused {
                return false;
            }
            state.deadline = Instant::now() + state.remaining;
            state.paused = false;
            true
        });
    }
}

impl Drop for HandshakeGuard {
    fn drop(&mut self) {
        self.0.state.send_if_modified(|state| {
            if !state.active {
                return false;
            }
            state.active = false;
            true
        });
    }
}
