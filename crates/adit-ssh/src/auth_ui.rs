//! A reply belongs to one challenge, not whichever session is active later.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CHALLENGE: AtomicU64 = AtomicU64::new(1);
pub(super) type Reply = Arc<Mutex<Option<oneshot::Sender<Vec<String>>>>>;

impl AuthPromptRequest {
    fn channel(
        name: String,
        instructions: String,
        prompts: Vec<AuthPromptField>,
    ) -> (Self, oneshot::Receiver<Vec<String>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                id: NEXT_CHALLENGE.fetch_add(1, Ordering::Relaxed),
                name,
                instructions,
                prompts,
                reply: Arc::new(Mutex::new(Some(tx))),
            },
            rx,
        )
    }

    /// False after answer, disconnect, timeout or cancellation. GUI polling can
    /// discard a stale modal without routing its answers into a newer challenge.
    pub fn is_pending(&self) -> bool {
        self.reply
            .lock()
            .ok()
            .is_some_and(|reply| reply.as_ref().is_some_and(|tx| !tx.is_closed()))
    }

    pub fn respond(&self, answers: Vec<String>) -> bool {
        self.reply
            .lock()
            .ok()
            .and_then(|mut reply| reply.take())
            .is_some_and(|tx| tx.send(answers).is_ok())
    }
}

impl std::fmt::Debug for AuthPromptRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPromptRequest")
            .field("id", &self.id)
            .field("pending", &self.is_pending())
            .finish_non_exhaustive()
    }
}

pub(super) enum AuthEventSink<'a> {
    Shell(&'a mpsc::Sender<LiveShellEvent>),
    Sftp(&'a mpsc::Sender<SftpEvent>),
    Tunnel(&'a mpsc::Sender<TunnelEvent>),
}

impl AuthEventSink<'_> {
    fn prompt(&self, prompt: AuthPromptRequest) -> Result<(), SshError> {
        let sent = match self {
            Self::Shell(tx) => tx.send(LiveShellEvent::AuthPrompt(prompt)).is_ok(),
            Self::Sftp(tx) => tx.send(SftpEvent::AuthPrompt(prompt)).is_ok(),
            Self::Tunnel(tx) => tx.send(TunnelEvent::AuthPrompt(prompt)).is_ok(),
        };
        if sent {
            Ok(())
        } else {
            Err(SshError::AuthenticationCancelled)
        }
    }
}

impl InteractiveKbd<'_> {
    pub(super) async fn ask(
        &mut self,
        name: &str,
        instructions: &str,
        prompts: Vec<AuthPromptField>,
    ) -> Result<Vec<String>, SshError> {
        let instructions = format!("{}\n{}", self.endpoint, instructions);
        let (prompt, reply) = AuthPromptRequest::channel(name.to_owned(), instructions, prompts);
        let expected = prompt.prompts.len();
        self.events.prompt(prompt)?;
        let answers = tokio::select! {
            answer = reply => answer.map_err(|_| SshError::AuthenticationCancelled)?,
            // Keep old command clients compatible; GUI replies use the scoped
            // oneshot instead. Auxiliary transports have no legacy reply queue.
            answer = async {
                match self.responses.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => answer.ok_or(SshError::AuthenticationCancelled)?,
        };
        if answers.is_empty() {
            return Err(SshError::AuthenticationCancelled);
        }
        if answers.len() != expected {
            return Err(SshError::InteractiveAuthenticationRequired);
        }
        Ok(answers)
    }
}

/// Resolved only in memory. An explicitly configured owner missing from this
/// map must fail, never fall back to the destination's password or private key.
#[derive(Clone)]
pub struct JumpCredential {
    pub username: String,
    pub password: String,
    pub auth: AuthOptions,
}

impl std::fmt::Debug for JumpCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JumpCredential")
            .field("auth", &self.auth)
            .field("secrets", &"<redacted>")
            .finish_non_exhaustive()
    }
}

pub(super) fn hop_credentials(
    hop: &JumpHop,
    resolved: &JumpCredentials,
    target_username: &str,
) -> Result<JumpCredential, SshError> {
    let mut credential = if let Some(id) = hop.auth_profile {
        resolved
            .get(&id)
            .ok_or(SshError::MissingJumpCredential(id))?
            .clone()
    } else {
        // Old profiles remain usable without disclosing a destination secret
        // to a different server. Choosing the target as owner is explicit reuse.
        JumpCredential {
            username: target_username.to_owned(),
            password: String::new(),
            auth: AuthOptions::default(),
        }
    };
    if !hop.username.trim().is_empty() {
        credential.username.clone_from(&hop.username);
    }
    Ok(credential)
}

impl std::fmt::Debug for AuthOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthOptions")
            .field("try_password", &self.try_password)
            .field("try_agent", &self.try_agent)
            .field("try_default_keys", &self.try_default_keys)
            .field("identity_file", &self.identity_file)
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl std::fmt::Debug for LiveShellRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveShellRequest")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth)
            .field("secrets", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SftpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpRequest")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth)
            .field("secrets", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for TunnelRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelRequest")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth)
            .field("secrets", &"<redacted>")
            .finish_non_exhaustive()
    }
}

// A rejected key must not silently downgrade a Key/Agent-only profile to a
// password conversation. Only an accepted first factor can request MFA here.
pub(super) fn key_auth_result(auth: client::AuthResult) -> Result<bool, SshError> {
    match auth {
        client::AuthResult::Success => Ok(true),
        client::AuthResult::Failure {
            partial_success: true,
            ..
        } => Err(SshError::AdditionalFactorRequired),
        client::AuthResult::Failure { .. } => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_domain::ProfileId;

    fn password_auth() -> AuthOptions {
        AuthOptions {
            try_password: true,
            try_agent: false,
            try_default_keys: false,
            identity_file: None,
            passphrase: None,
            agent_socket: None,
        }
    }

    #[test]
    fn explicit_owners_never_receive_target_password_key_or_passphrase() {
        let first = ProfileId::new();
        let second = ProfileId::new();
        let mut resolved = JumpCredentials::new();
        let mut key_auth = password_auth();
        key_auth.try_password = false;
        key_auth.identity_file = Some("fixture-hop-two.key".into());
        key_auth.passphrase = Some("HOP-TWO-PASSPHRASE".into());
        resolved.insert(
            first,
            JumpCredential {
                username: "first-user".into(),
                password: "HOP-ONE-PASSWORD".into(),
                auth: password_auth(),
            },
        );
        resolved.insert(
            second,
            JumpCredential {
                username: "second-user".into(),
                password: String::new(),
                auth: key_auth,
            },
        );
        let mut target = password_auth();
        target.identity_file = Some("fixture-target.key".into());
        target.passphrase = Some("TARGET-PASSPHRASE".into());
        for (owner, password, user) in [
            (first, "HOP-ONE-PASSWORD", "first-user"),
            (second, "", "second-user"),
        ] {
            let hop = JumpHop {
                host: "fixture.invalid".into(),
                port: 22,
                username: String::new(),
                auth_profile: Some(owner),
            };
            let actual = hop_credentials(&hop, &resolved, "target-user").unwrap();
            assert_eq!(
                (actual.username.as_str(), actual.password.as_str()),
                (user, password)
            );
            assert_ne!(actual.auth.identity_file, target.identity_file);
            assert_ne!(actual.auth.passphrase, target.passphrase);
        }
        let debug = format!("{resolved:?}");
        assert!(!debug.contains("HOP-ONE-PASSWORD"));
        assert!(!debug.contains("HOP-TWO-PASSPHRASE"));
        assert!(!format!(
            "{:?}",
            LiveShellCommand::AuthResponses(vec!["OTP-SECRET".into()])
        )
        .contains("OTP-SECRET"));
    }

    #[test]
    fn legacy_hops_never_inherit_target_secrets_and_missing_owners_fail_closed() {
        let mut hop = JumpHop::parse("bastion.invalid").unwrap();
        let resolved = JumpCredentials::new();
        let got = hop_credentials(&hop, &resolved, "target-user").unwrap();
        assert_eq!(got.username, "target-user");
        assert!(got.password.is_empty());
        assert!(got.auth.identity_file.is_none());
        assert!(got.auth.passphrase.is_none());
        hop.auth_profile = Some(ProfileId::new());
        assert!(matches!(
            hop_credentials(&hop, &resolved, "target-user"),
            Err(SshError::MissingJumpCredential(_))
        ));
    }

    #[test]
    fn rejected_keys_do_not_downgrade_but_accepted_partial_auth_requests_mfa() {
        assert!(!key_auth_result(client::AuthResult::Failure {
            remaining_methods: russh::MethodSet::empty(),
            partial_success: false,
        })
        .unwrap());
        assert!(matches!(
            key_auth_result(client::AuthResult::Failure {
                remaining_methods: russh::MethodSet::empty(),
                partial_success: true,
            }),
            Err(SshError::AdditionalFactorRequired)
        ));
        assert!(key_auth_result(client::AuthResult::Success).unwrap());
    }

    async fn next_prompt(rx: &mpsc::Receiver<SftpEvent>) -> AuthPromptRequest {
        loop {
            match rx.try_recv() {
                Ok(SftpEvent::AuthPrompt(p)) => return p,
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                Err(e) => panic!("fixture event channel closed: {e}"),
            }
        }
    }

    #[test]
    fn two_hops_get_distinct_challenges_and_old_answers_cannot_cross_rounds() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (events, rx) = mpsc::channel();
            let mut kbd = InteractiveKbd {
                events: AuthEventSink::Sftp(&events),
                endpoint: "hop-one".into(),
                responses: None,
            };
            let prompts = [client::Prompt {
                prompt: "Verification code: ".into(),
                echo: false,
            }];
            let producer = async {
                let one = keyboard_interactive_round_answers(
                    "",
                    "",
                    &prompts,
                    "NOT-AN-OTP",
                    None,
                    Some(&mut kbd),
                )
                .await
                .unwrap();
                kbd.endpoint = "hop-two".into();
                let two = keyboard_interactive_round_answers(
                    "",
                    "",
                    &prompts,
                    "ALSO-NOT-AN-OTP",
                    None,
                    Some(&mut kbd),
                )
                .await
                .unwrap();
                assert_eq!(one, ["FIRST-FACTOR"]);
                assert_eq!(two, ["SECOND-FACTOR"]);
            };
            let consumer = async {
                let first = next_prompt(&rx).await;
                assert!(first.instructions.contains("hop-one"));
                assert!(first.respond(vec!["FIRST-FACTOR".into()]));
                let second = next_prompt(&rx).await;
                assert_ne!(first.id, second.id);
                assert!(second.instructions.contains("hop-two"));
                assert!(!first.respond(vec!["MUST-NOT-REACH-HOP-TWO".into()]));
                assert!(second.respond(vec!["SECOND-FACTOR".into()]));
            };
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(producer, consumer);
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn cancel_malformed_answer_and_disconnected_challenge_are_explicit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for answers in [vec![], vec!["EXTRA".into(), "ANSWER".into()]] {
                let (events, rx) = mpsc::channel::<TunnelEvent>();
                let mut kbd = InteractiveKbd {
                    events: AuthEventSink::Tunnel(&events),
                    endpoint: "fixture".into(),
                    responses: None,
                };
                let producer = kbd.ask(
                    "",
                    "",
                    vec![AuthPromptField {
                        prompt: "OTP".into(),
                        echo: false,
                    }],
                );
                let consumer = async {
                    loop {
                        if let Ok(TunnelEvent::AuthPrompt(p)) = rx.try_recv() {
                            assert!(p.respond(answers));
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                };
                let (result, _) = tokio::join!(producer, consumer);
                assert!(matches!(
                    result,
                    Err(SshError::AuthenticationCancelled
                        | SshError::InteractiveAuthenticationRequired)
                ));
            }
            let (prompt, response) = AuthPromptRequest::channel("".into(), "".into(), Vec::new());
            drop(response);
            assert!(!prompt.is_pending());
            assert!(!prompt.respond(vec!["LATE-ANSWER".into()]));
        });
    }
}
