//! A network handshake budget which excludes time spent awaiting host-key trust.
use super::{effective_timeout_secs, SshError};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
struct State {
    active: bool,
    paused: bool,
    deadline: Instant,
    remaining: Duration,
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
            let state = *changes.borrow_and_update();
            if !state.active {
                return;
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
                    let current = *changes.borrow_and_update();
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
