use super::*;

struct Budget {
    delays: Vec<Duration>,
    used: usize,
    ever_listened: bool,
}

impl Budget {
    fn new(delays: &[Duration]) -> Self {
        Self {
            delays: delays
                .iter()
                .take(100)
                .map(|d| (*d).min(Duration::from_secs(3600)))
                .collect(),
            used: 0,
            ever_listened: false,
        }
    }

    fn next(&mut self, error: &SshError) -> Option<Duration> {
        if !self.ever_listened || !retryable(error) {
            return None;
        }
        let delay = *self.delays.get(self.used)?;
        self.used += 1;
        // Do not reset on Listening: a flapping server could otherwise consume
        // an unbounded number of logins while briefly rebinding each time.
        Some(delay)
    }
}

fn retryable(error: &SshError) -> bool {
    match error {
        SshError::Timeout(_) => true,
        SshError::Russh(error) => matches!(
            error,
            russh::Error::IO(_)
                | russh::Error::Disconnect
                | russh::Error::HUP
                | russh::Error::ConnectionTimeout
                | russh::Error::KeepaliveTimeout
                | russh::Error::InactivityTimeout
                | russh::Error::SendError
        ),
        // Credentials, key verification, local bind/forward refusal and runtime
        // configuration failures require a user decision, never another login.
        _ => false,
    }
}

pub(super) async fn run(
    request: TunnelRequest,
    mut shared: Option<SharedSession>,
    mut commands: tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: &mpsc::Sender<TunnelEvent>,
    mut retry_changes: tokio::sync::watch::Receiver<bool>,
) {
    let mut budget = Budget::new(&request.reconnect_delays);
    loop {
        if commands.is_closed() || commands.try_recv().is_ok() {
            return;
        }
        let (result, listened) =
            attempt(request.clone(), shared.take(), &mut commands, events).await;
        budget.ever_listened |= listened;
        let Err(error) = result else { return };
        let _ = events.send(TunnelEvent::Error(error.to_string()));
        if !*retry_changes.borrow() {
            return;
        }
        let Some(delay) = budget.next(&error) else {
            if budget.ever_listened
                && retryable(&error)
                && !budget.delays.is_empty()
                && budget.used == budget.delays.len()
            {
                let _ = events.send(TunnelEvent::RetryExhausted {
                    attempts: budget.used as u32,
                });
            }
            return;
        };
        let _ = events.send(TunnelEvent::Reconnecting {
            attempt: budget.used as u32,
            max_attempts: budget.delays.len() as u32,
            delay_secs: delay.as_secs(),
        });
        if !wait(delay, &mut commands, &mut retry_changes).await {
            return;
        }
    }
}

async fn wait(
    delay: Duration,
    commands: &mut tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    enabled: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let timer = tokio::time::sleep(delay);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            biased;
            _ = commands.recv() => return false,
            changed = enabled.changed() => {
                if changed.is_err() || !*enabled.borrow() { return false; }
            }
            _ = &mut timer => return *enabled.borrow(),
        }
    }
}

// Each attempt gets its own event queue. Remote-forward tasks run on a shared
// connection's runtime and can finish late; their Closed/Error must not corrupt
// the new attempt's active count or revive an old authentication prompt.
async fn attempt(
    request: TunnelRequest,
    shared: Option<SharedSession>,
    commands: &mut tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: &mpsc::Sender<TunnelEvent>,
) -> (Result<(), SshError>, bool) {
    let (tx, rx) = mpsc::channel();
    let work = run_tunnel_session(request, shared, commands, tx);
    tokio::pin!(work);
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    let mut listened = false;
    loop {
        tokio::select! {
            result = &mut work => {
                drain(&rx, events, &mut listened);
                return (result, listened);
            }
            _ = poll.tick() => drain(&rx, events, &mut listened),
        }
    }
}

fn drain(
    rx: &mpsc::Receiver<TunnelEvent>,
    events: &mpsc::Sender<TunnelEvent>,
    listened: &mut bool,
) {
    while let Ok(event) = rx.try_recv() {
        *listened |= matches!(event, TunnelEvent::Listening { .. });
        let _ = events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_requires_a_listener_and_never_resets_for_a_flapping_server() {
        let mut budget = Budget::new(&[Duration::from_secs(1), Duration::from_secs(2)]);
        let dropped = SshError::Russh(russh::Error::Disconnect);
        assert_eq!(budget.next(&dropped), None);
        budget.ever_listened = true;
        assert_eq!(budget.next(&dropped), Some(Duration::from_secs(1)));
        assert_eq!(budget.next(&dropped), Some(Duration::from_secs(2)));
        assert_eq!(budget.next(&dropped), None);
    }

    #[test]
    fn security_authentication_and_configuration_errors_never_redial() {
        for error in [
            SshError::HostKeyRejected,
            SshError::HostKeyChanged {
                host: "fixture".into(),
                expected: "old".into(),
                actual: "new".into(),
                known_hosts_path: "fixture".into(),
            },
            SshError::AuthenticationCancelled,
            SshError::AuthenticationRejected,
            SshError::NoAuthenticationMethod,
            SshError::KeyPassphraseWrong("fixture".into()),
            SshError::KnownHosts("fixture".into()),
            SshError::Tunnel("bind refused".into()),
            SshError::Russh(russh::Error::KeyChanged { line: 1 }),
            SshError::JumpAuthentication {
                index: 1,
                host: "hop".into(),
                source: Box::new(SshError::AuthenticationRejected),
            },
            SshError::JumpAuthentication {
                index: 1,
                host: "hop".into(),
                source: Box::new(SshError::Russh(russh::Error::Disconnect)),
            },
            SshError::InteractiveAuthenticationRequired,
            SshError::AdditionalFactorRequired,
            SshError::MissingJumpCredential(adit_domain::ProfileId::new()),
        ] {
            let mut budget = Budget::new(&[Duration::ZERO]);
            budget.ever_listened = true;
            assert!(budget.next(&error).is_none(), "{error}");
        }
    }

    #[test]
    fn a_long_backoff_is_cancelled_by_disconnect_drop_or_policy_disable() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for mode in 0..3 {
                let (tx, mut rx) = tokio_mpsc::unbounded_channel();
                let mut tx = Some(tx);
                let (enabled, mut changes) = tokio::sync::watch::channel(true);
                let cancel = async {
                    tokio::task::yield_now().await;
                    match mode {
                        0 => {
                            tx.as_ref()
                                .unwrap()
                                .send(TunnelCommand::Disconnect)
                                .unwrap();
                        }
                        1 => drop(tx.take()),
                        _ => {
                            enabled.send_replace(false);
                        }
                    }
                };
                let waiting = wait(Duration::from_secs(3600), &mut rx, &mut changes);
                let (result, _) = tokio::time::timeout(Duration::from_secs(1), async {
                    tokio::join!(waiting, cancel)
                })
                .await
                .unwrap();
                assert!(!result);
            }
        });
    }
}
