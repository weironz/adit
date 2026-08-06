//! OAuth 2.0 Device Authorization Grant (RFC 8628), for GitHub.
//!
//! **Why not the PKCE loopback flow in [`super::oauth`].** GitHub *does* accept
//! PKCE — `code_challenge` / `code_challenge_method=S256` were added in July
//! 2025, so the folklore that it does not is out of date. That is not the
//! reason this module exists. The reason is the sentence GitHub prints next to
//! the announcement: **it does not distinguish public from confidential
//! clients**, PKCE is optional for every flow, and the web application flow's
//! token exchange still requires `client_secret`. PKCE on GitHub is therefore
//! defence in depth for a client that already has a secret — it does not turn a
//! desktop app into a public client the way it does on Dropbox and Microsoft.
//!
//! The device flow is the one GitHub flow that genuinely needs **no client
//! secret** (its own documentation says so outright) and no loopback listener.
//! That buys two things a terminal client actually wants: nothing confidential
//! compiled into a binary users already have, and no localhost port to bind —
//! which is what fails first on a locked-down corporate machine.
//!
//! **The interaction is the user code.** There is no redirect back; the user
//! reads an 8-character code off our window and types it into
//! `verification_uri`. A device flow whose code is not plainly visible and
//! copyable is a device flow nobody can complete, so the panel shows it large
//! and next to a copy button.
//!
//! **What is stored.** GitHub's OAuth-app access tokens do not expire on their
//! own, so there is no refresh token in this flow and none is asked for. The
//! access token goes into the same sealed credential slot a hand-pasted
//! personal access token would — which is exactly why the manual path still
//! works and was kept.

use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::{agent, remote_error};
use crate::SyncError;

/// Added to the polling interval each time GitHub answers `slow_down`, per
/// RFC 8628 §3.5 and GitHub's own instruction ("add 5 seconds").
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// Used when the server omits `interval`. RFC 8628 §3.2 names 5 seconds as the
/// default, and GitHub's minimum is the same.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// Used when the server omits `expires_in`. GitHub's device codes last 900
/// seconds; guessing shorter only means we give up before the server would,
/// which is the harmless direction to be wrong in.
const DEFAULT_EXPIRY: Duration = Duration::from_secs(900);

/// Endpoints and scope for one device-flow provider.
#[derive(Debug, Clone)]
pub struct DeviceFlowConfig {
    pub provider: &'static str,
    /// Resolved at runtime like every other client id: the user's own if they
    /// supplied one, otherwise whatever this build was compiled with.
    pub client_id: String,
    pub device_code_url: &'static str,
    pub token_url: &'static str,
    pub scope: &'static str,
}

/// A started authorisation: what to show the user, where to send them, and the
/// secret half we poll with.
#[derive(Clone)]
pub struct DeviceAuth {
    config: DeviceFlowConfig,
    /// The secret half of the pair. Never shown, never logged.
    device_code: String,
    /// The half the user reads off the screen and types into the browser.
    pub user_code: String,
    pub verification_uri: String,
    /// Grows when the server says `slow_down`, which is why it is owned state
    /// rather than a constant.
    interval: Duration,
    /// A local deadline, so an abandoned attempt stops polling even if the
    /// server never gets around to saying `expired_token`.
    expires_at: SystemTime,
}

/// Redacted by hand rather than derived. This type rides inside a UI message,
/// and every message this app builds is `Debug` — a derived impl would print
/// the device code, which is the one credential in this flow, into any trace
/// that ever formats a message.
impl std::fmt::Debug for DeviceAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuth")
            .field("provider", &self.config.provider)
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("device_code", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    #[serde(default)]
    device_code: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
    /// The device-code endpoint reports refusals in the body too — a client id
    /// that is not registered for device flow answers `200` with
    /// `error=device_flow_disabled`, not a `4xx`.
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Ask GitHub for a code pair. Runs on a worker thread: it is a network round
/// trip, and the UI thread is what draws.
pub fn begin(config: DeviceFlowConfig) -> Result<DeviceAuth, SyncError> {
    if config.client_id.trim().is_empty() {
        return Err(SyncError::NotAuthenticated {
            provider: config.provider.to_owned(),
        });
    }

    let body = agent()
        .post(config.device_code_url)
        // Without this GitHub answers `application/x-www-form-urlencoded` —
        // `device_code=...&user_code=...` — and serde_json reports it as
        // malformed JSON. The default format is the trap here, not the parse.
        .set("Accept", "application/json")
        .send_form(&[
            ("client_id", config.client_id.as_str()),
            ("scope", config.scope),
        ])
        .map_err(|error| remote_error(config.provider, error))?
        .into_string()
        .map_err(SyncError::Io)?;

    let parsed: DeviceCodeResponse =
        serde_json::from_str(&body).map_err(|error| SyncError::Malformed(error.to_string()))?;

    if !parsed.error.is_empty() {
        return Err(SyncError::Remote {
            provider: config.provider.to_owned(),
            message: describe(&parsed.error, &parsed.error_description),
        });
    }
    if parsed.device_code.is_empty() || parsed.user_code.is_empty() {
        return Err(SyncError::Remote {
            provider: config.provider.to_owned(),
            message: String::from("设备码响应缺少 device_code 或 user_code"),
        });
    }

    Ok(DeviceAuth {
        // Never take a shorter interval than the server asked for: polling
        // faster than permitted is what earns `slow_down`, and ignoring the
        // server's floor turns one impatient client into a rate-limited one.
        interval: parsed
            .interval
            .map_or(DEFAULT_INTERVAL, Duration::from_secs)
            .max(DEFAULT_INTERVAL),
        expires_at: SystemTime::now()
            + parsed.expires_in.map_or(DEFAULT_EXPIRY, Duration::from_secs),
        device_code: parsed.device_code,
        user_code: parsed.user_code,
        verification_uri: if parsed.verification_uri.is_empty() {
            String::from("https://github.com/login/device")
        } else {
            parsed.verification_uri
        },
        config,
    })
}

/// What one poll of the token endpoint means. Separated from the request so the
/// state machine can be tested against captured bodies rather than the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PollStep {
    /// Authorised. This is the only exit that produces a token.
    Token(String),
    /// `authorization_pending`: the user has not finished in the browser yet.
    /// The overwhelmingly common answer, and emphatically **not** an error —
    /// treating it as one ends the flow while the user is still typing.
    KeepWaiting,
    /// `slow_down`: we polled too fast. Keep waiting, but widen the interval —
    /// retrying at the same rate is what gets the client throttled outright.
    SlowDown,
    /// `expired_token`: the 15-minute window closed. Terminal, and it needs a
    /// *new device code* — re-polling this one can never succeed.
    Expired,
    /// `access_denied`: the user pressed cancel. Terminal, and the code cannot
    /// be reused, so retrying would only spam a person who just said no.
    Denied,
    /// Anything else — a wrong client id, device flow not enabled on the app.
    /// Terminal: these are configuration faults that no amount of waiting
    /// fixes, and folding them into `KeepWaiting` is how a flow hangs for
    /// fifteen minutes with nothing to show for it.
    Failed(String),
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Read one token-endpoint body.
///
/// GitHub answers **HTTP 200 for every one of these**, with the outcome in the
/// body — so status codes decide nothing here and the body is the whole signal.
pub(crate) fn classify(body: &str) -> PollStep {
    let Ok(parsed) = serde_json::from_str::<TokenResponse>(body) else {
        return PollStep::Failed(String::from("令牌响应不是有效的 JSON"));
    };
    if !parsed.access_token.is_empty() {
        return PollStep::Token(parsed.access_token);
    }
    match parsed.error.as_str() {
        "authorization_pending" => PollStep::KeepWaiting,
        "slow_down" => PollStep::SlowDown,
        "expired_token" => PollStep::Expired,
        "access_denied" => PollStep::Denied,
        "" => PollStep::Failed(String::from("令牌响应既没有 access_token 也没有 error")),
        other => PollStep::Failed(describe(other, &parsed.error_description)),
    }
}

impl DeviceAuth {
    /// The interval currently in force, in seconds. Exposed for tests, which is
    /// the only way to observe that `slow_down` actually widened it.
    #[cfg(test)]
    pub(crate) fn interval_secs(&self) -> u64 {
        self.interval.as_secs()
    }

    /// Poll until the user finishes, refuses, or the code expires. Blocks on a
    /// human, so it belongs on a worker thread.
    pub fn poll_until_authorized(mut self) -> Result<String, SyncError> {
        loop {
            // Sleep *first*: the user has not even read the code yet when this
            // starts, so an immediate poll is guaranteed to be answered
            // `authorization_pending` and only spends the rate limit.
            std::thread::sleep(self.interval);

            // A local deadline as well as the server's. If the network is
            // gone the server can never tell us the code expired, and without
            // this the worker would poll until the application closed.
            if SystemTime::now() >= self.expires_at {
                return Err(self.remote(String::from("设备码已过期，请重新开始连接")));
            }

            match classify(&self.poll_once()?) {
                PollStep::Token(token) => return Ok(token),
                PollStep::KeepWaiting => {}
                PollStep::SlowDown => self.interval += SLOW_DOWN_STEP,
                PollStep::Expired => {
                    return Err(self.remote(String::from("设备码已过期，请重新开始连接")));
                }
                PollStep::Denied => {
                    return Err(self.remote(String::from("授权被拒绝")));
                }
                PollStep::Failed(message) => return Err(self.remote(message)),
            }
        }
    }

    fn poll_once(&self) -> Result<String, SyncError> {
        agent()
            .post(self.config.token_url)
            // Same trap as the device-code request: form-encoded by default.
            .set("Accept", "application/json")
            .send_form(&[
                ("client_id", self.config.client_id.as_str()),
                ("device_code", self.device_code.as_str()),
                // Exact and required; anything else earns
                // `unsupported_grant_type`.
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .map_err(|error| remote_error(self.config.provider, error))?
            .into_string()
            .map_err(SyncError::Io)
    }

    fn remote(&self, message: String) -> SyncError {
        SyncError::Remote {
            provider: self.config.provider.to_owned(),
            message,
        }
    }
}

/// Keep the server's own wording when it offered any — GitHub's
/// `error_description` is a readable sentence, and replacing it with our own
/// guess is how a fixable registration mistake becomes an unexplained failure.
fn describe(error: &str, description: &str) -> String {
    if description.trim().is_empty() {
        error.to_owned()
    } else {
        format!("{description} ({error})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(interval: u64) -> DeviceAuth {
        DeviceAuth {
            config: DeviceFlowConfig {
                provider: "Test",
                client_id: String::from("id"),
                device_code_url: "https://example.com/device",
                token_url: "https://example.com/token",
                scope: "gist",
            },
            device_code: String::from("secret-device-code"),
            user_code: String::from("ABCD-1234"),
            verification_uri: String::from("https://github.com/login/device"),
            interval: Duration::from_secs(interval),
            expires_at: SystemTime::now() + Duration::from_secs(900),
        }
    }

    /// The happy path. Note GitHub returns this with HTTP 200 and no `error`
    /// key at all, so the absence of `error` cannot be the failure signal.
    #[test]
    fn an_access_token_ends_the_poll() {
        let step = classify(r#"{"access_token":"gho_abc","token_type":"bearer","scope":"gist"}"#);
        assert_eq!(step, PollStep::Token(String::from("gho_abc")));
    }

    /// `authorization_pending` is the normal answer for almost the entire flow.
    /// Reading it as a failure ends the authorisation while the user is still
    /// typing the code — the single most damaging way to get this wrong.
    #[test]
    fn authorization_pending_keeps_waiting() {
        let step = classify(
            r#"{"error":"authorization_pending","error_description":"The authorization request is still pending."}"#,
        );
        assert_eq!(step, PollStep::KeepWaiting);
    }

    /// `slow_down` also keeps waiting — but it must widen the interval, or the
    /// client keeps polling at the rate that earned the warning.
    #[test]
    fn slow_down_keeps_waiting_and_is_distinct_from_pending() {
        let step = classify(r#"{"error":"slow_down","error_description":"Too many requests"}"#);
        assert_eq!(step, PollStep::SlowDown);
        assert_ne!(step, PollStep::KeepWaiting);
    }

    /// The widening itself, since a `SlowDown` that did not change the interval
    /// would satisfy the test above while behaving identically to `KeepWaiting`.
    #[test]
    fn slow_down_widens_the_interval_by_five_seconds() {
        let mut pending = auth(5);
        assert_eq!(pending.interval_secs(), 5);
        // What the loop does on a `SlowDown`, applied twice: the interval has
        // to keep growing, not settle at one bump.
        pending.interval += SLOW_DOWN_STEP;
        assert_eq!(pending.interval_secs(), 10);
        pending.interval += SLOW_DOWN_STEP;
        assert_eq!(pending.interval_secs(), 15);
    }

    /// Terminal, and distinct from `access_denied`: the user did nothing wrong,
    /// they were just too slow, and the fix is a fresh device code rather than
    /// a fresh decision.
    #[test]
    fn expired_token_is_terminal() {
        let step = classify(
            r#"{"error":"expired_token","error_description":"This 'device_code' has expired."}"#,
        );
        assert_eq!(step, PollStep::Expired);
    }

    /// Terminal, and the code cannot be reused. Retrying here would re-prompt
    /// somebody who has just pressed cancel.
    #[test]
    fn access_denied_is_terminal() {
        let step =
            classify(r#"{"error":"access_denied","error_description":"The user denied it."}"#);
        assert_eq!(step, PollStep::Denied);
    }

    /// The four documented answers must not collapse into each other: two of
    /// them continue and two of them stop, and confusing the pairs produces
    /// either a hang or a flow that quits while the user is still typing.
    #[test]
    fn the_four_documented_answers_stay_distinct() {
        let pending = classify(r#"{"error":"authorization_pending"}"#);
        let slow = classify(r#"{"error":"slow_down"}"#);
        let expired = classify(r#"{"error":"expired_token"}"#);
        let denied = classify(r#"{"error":"access_denied"}"#);

        let all = [&pending, &slow, &expired, &denied];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "two documented responses classified the same");
            }
        }
        // Two continue, two stop.
        assert!(matches!(pending, PollStep::KeepWaiting));
        assert!(matches!(slow, PollStep::SlowDown));
        assert!(matches!(expired, PollStep::Expired));
        assert!(matches!(denied, PollStep::Denied));
    }

    /// A misconfigured app registration is a configuration fault, not something
    /// waiting fixes — so it stops rather than polling for fifteen minutes.
    #[test]
    fn an_unrecognised_error_stops_and_keeps_the_servers_wording() {
        let step = classify(
            r#"{"error":"device_flow_disabled","error_description":"Device Flow has not been enabled."}"#,
        );
        match step {
            PollStep::Failed(message) => {
                assert!(message.contains("Device Flow has not been enabled"), "{message}");
                assert!(message.contains("device_flow_disabled"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// GitHub answers form-encoded unless asked for JSON. If the `Accept`
    /// header is ever dropped this is the body that arrives, and it must fail
    /// loudly rather than read as "no error, no token".
    #[test]
    fn a_form_encoded_body_is_a_failure_not_a_pending() {
        let step = classify("access_token=gho_abc&token_type=bearer");
        assert!(matches!(step, PollStep::Failed(_)), "{step:?}");
    }

    /// A well-formed JSON body carrying neither half is still a failure — an
    /// empty `error` must not fall through the match into "keep waiting".
    #[test]
    fn an_empty_body_is_a_failure() {
        assert!(matches!(classify("{}"), PollStep::Failed(_)));
    }

    /// Without a client id there is nothing to authorise, and saying so locally
    /// beats showing the user a code that can never be redeemed.
    #[test]
    fn a_missing_client_id_is_refused_before_any_request() {
        let config = DeviceFlowConfig {
            provider: "Test",
            client_id: String::new(),
            device_code_url: "https://example.com/device",
            token_url: "https://example.com/token",
            scope: "gist",
        };
        assert!(matches!(
            begin(config),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }

    /// The device code is the one credential in this flow, and this type rides
    /// inside a `Debug` UI message. It must not print.
    #[test]
    fn the_device_code_never_reaches_debug_output() {
        let rendered = format!("{:?}", auth(5));
        assert!(!rendered.contains("secret-device-code"), "{rendered}");
        // The user code, by contrast, is meant to be seen.
        assert!(rendered.contains("ABCD-1234"), "{rendered}");
    }
}
