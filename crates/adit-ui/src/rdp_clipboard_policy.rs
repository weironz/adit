//! Presentation of the requested clipboard policy for one RDP session.

use super::*;
use adit_session::{RdpClipboardPolicyFailure, RdpClipboardPolicyStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PolicyDisplay {
    pub enabled: bool,
    pub phase: PolicyPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyPhase {
    Pending,
    Confirmed,
    Failed(RdpClipboardPolicyFailure),
}

/// Read a fresh snapshot for the session being painted. In particular, never
/// keep a global success notice: the manager resolves the current epoch for
/// this session, including after reconnects that start a new helper.
pub(crate) fn for_session(app: &AditApp, session: SessionId) -> Option<PolicyDisplay> {
    let status = app.manager.rdp_clipboard_policy_status(session)?;
    Some(from_status(app.rdp_clipboard, status))
}

pub(crate) fn for_selected_session(app: &AditApp) -> Option<PolicyDisplay> {
    if app.main_view == MainView::Hosts || !native_windows::owns_selected_session(app) {
        return None;
    }
    for_session(app, app.manager.active_session()?)
}

fn from_status(requested: bool, status: RdpClipboardPolicyStatus) -> PolicyDisplay {
    let (enabled, phase) = match status {
        RdpClipboardPolicyStatus::Pending { enabled, .. } => (enabled, PolicyPhase::Pending),
        RdpClipboardPolicyStatus::Confirmed { enabled, .. } => (enabled, PolicyPhase::Confirmed),
        RdpClipboardPolicyStatus::Failed {
            enabled, reason, ..
        } => (enabled, PolicyPhase::Failed(reason)),
    };
    PolicyDisplay {
        enabled: requested,
        // A snapshot for a different requested value cannot confirm this one.
        phase: if enabled == requested {
            phase
        } else {
            PolicyPhase::Pending
        },
    }
}

impl PolicyDisplay {
    pub(crate) fn label(self) -> &'static str {
        t(match (self.phase, self.enabled) {
            (PolicyPhase::Pending, true) => "剪贴板：开启请求中",
            (PolicyPhase::Pending, false) => "剪贴板：关闭请求中",
            (PolicyPhase::Confirmed, true) => "剪贴板：已确认开启",
            (PolicyPhase::Confirmed, false) => "剪贴板：已确认关闭",
            (PolicyPhase::Failed(_), true) => "剪贴板：开启请求失败",
            (PolicyPhase::Failed(_), false) => "剪贴板：关闭请求失败",
        })
    }

    pub(crate) fn is_failed(self) -> bool {
        matches!(self.phase, PolicyPhase::Failed(_))
    }

    pub(crate) fn inline(self) -> String {
        match self.phase {
            PolicyPhase::Failed(reason) => {
                format!("{} · {}", self.label(), t(failure_reason(reason)))
            }
            PolicyPhase::Confirmed => t(if self.enabled {
                "剪贴板：辅助进程已确认开启"
            } else {
                "剪贴板：辅助进程已确认关闭"
            })
            .to_owned(),
            PolicyPhase::Pending => self.label().to_owned(),
        }
    }

    fn detail(self) -> String {
        match self.phase {
            PolicyPhase::Confirmed => format!(
                "{} · {}",
                self.label(),
                t("辅助进程已观察请求且剪贴板通道就绪，不代表远端应用已接收内容。")
            ),
            _ => self.inline(),
        }
    }

    pub(crate) fn badge(self, position: tooltip::Position) -> Element<'static, Message> {
        let color = if self.is_failed() {
            danger()
        } else {
            muted_text()
        };
        tooltip(
            text(self.label()).size(11).color(color),
            container(text(self.detail()).size(11).color(primary_text()))
                .padding([5, 8])
                .style(|_| tooltip_style()),
            position,
        )
        .into()
    }
}

fn failure_reason(reason: RdpClipboardPolicyFailure) -> &'static str {
    match reason {
        RdpClipboardPolicyFailure::HelperDisconnected => "辅助进程已断开",
        RdpClipboardPolicyFailure::ChannelNotNegotiated => "剪贴板通道未协商",
        RdpClipboardPolicyFailure::ConfirmationTimedOut => "等待确认超时",
        RdpClipboardPolicyFailure::MappingUnavailable => "共享策略不可用",
        RdpClipboardPolicyFailure::HelperRejected => "辅助进程拒绝请求",
        RdpClipboardPolicyFailure::BufferLimit => "共享策略缓冲区已满",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_session::clipboard_test_fixture::SyntheticRdp;

    fn fixture() -> (AditApp, [SessionId; 2], [SyntheticRdp; 2]) {
        let profiles: Vec<_> = (0..2)
            .map(|n| {
                ConnectionProfile::new(format!("policy-{n}"), "fixture.invalid", 22, "fixture")
            })
            .collect();
        let mut manager = SessionManager::with_profiles(profiles);
        let ids: Vec<_> = manager.profiles().iter().map(|p| p.id).collect();
        let sessions = std::array::from_fn(|n| manager.open_mock_session(ids[n]).unwrap());
        let peers = std::array::from_fn(|n| SyntheticRdp::attach(&mut manager, sessions[n]));
        let app = AditApp {
            manager,
            main_view: MainView::Terminal,
            rdp_clipboard: true,
            ..AditApp::default()
        };
        (app, sessions, peers)
    }

    fn request(app: &mut AditApp, enabled: bool) {
        let _ = update(app, Message::ToggleRdpClipboard(enabled));
    }

    #[test]
    fn out_of_order_acknowledgements_remain_scoped_to_session_and_current_epoch() {
        let (mut app, sessions, peers) = fixture();
        for peer in &peers {
            assert_eq!(peer.prepare_policy(&app.manager).unwrap(), 0);
        }
        peers[0].acknowledge_policy(&app.manager, 0, true);
        assert_eq!(
            for_session(&app, sessions[0]).unwrap().phase,
            PolicyPhase::Pending
        );
        peers[0].policy_channel_ready(&app.manager, 1);
        assert_eq!(
            for_session(&app, sessions[0]).unwrap().phase,
            PolicyPhase::Confirmed
        );
        assert_eq!(
            for_session(&app, sessions[1]).unwrap().phase,
            PolicyPhase::Pending
        );

        request(&mut app, false);
        request(&mut app, true);
        peers[0].acknowledge_policy(&app.manager, 1, false);
        peers[0].acknowledge_policy(&app.manager, 0, true);
        let pending = for_session(&app, sessions[0]).unwrap();
        assert_eq!(
            (pending.enabled, pending.phase),
            (true, PolicyPhase::Pending)
        );
        assert_eq!(
            app.manager.rdp_clipboard_policy_status(sessions[0]),
            Some(RdpClipboardPolicyStatus::Pending {
                epoch: 2,
                enabled: true
            })
        );

        peers[0].acknowledge_policy(&app.manager, 2, true);
        assert_eq!(
            for_session(&app, sessions[0]).unwrap().phase,
            PolicyPhase::Confirmed
        );
        assert_eq!(
            for_session(&app, sessions[1]).unwrap().phase,
            PolicyPhase::Pending
        );
        peers[0].acknowledge_policy(&app.manager, 1, false);
        assert_eq!(
            for_session(&app, sessions[0]).unwrap().phase,
            PolicyPhase::Confirmed
        );
        assert_eq!(app.manager.sessions().len(), 2);
        assert!(sessions.into_iter().all(|session| app
            .manager
            .session_summary(session)
            .is_some_and(|summary| summary.status == SessionStatus::Connected)));
        for peer in &peers {
            while let Some(command) = peer.try_command() {
                assert!(!matches!(command, adit_rdp_proto::ClientMsg::Close));
            }
        }
    }

    #[test]
    fn timeout_disconnect_and_mismatched_request_never_show_confirmation() {
        let (mut app, sessions, peers) = fixture();
        peers[0].prepare_policy(&app.manager).unwrap();
        peers[0].policy_channel_ready(&app.manager, 1);
        peers[0].acknowledge_policy(&app.manager, 0, true);
        request(&mut app, false);
        peers[0].timeout_policy(&app.manager);
        peers[0].acknowledge_policy(&app.manager, 0, true);
        let timed_out = for_session(&app, sessions[0]).unwrap();
        assert!(!timed_out.enabled);
        assert_eq!(
            timed_out.phase,
            PolicyPhase::Failed(RdpClipboardPolicyFailure::ConfirmationTimedOut)
        );
        assert_eq!(
            app.manager.rdp_clipboard_policy_status(sessions[0]),
            Some(RdpClipboardPolicyStatus::Failed {
                epoch: 1,
                enabled: false,
                reason: RdpClipboardPolicyFailure::ConfirmationTimedOut,
            })
        );
        peers[0].acknowledge_policy(&app.manager, 1, false);
        let confirmed = for_session(&app, sessions[0]).unwrap();
        assert_eq!(
            (confirmed.enabled, confirmed.phase),
            (false, PolicyPhase::Confirmed)
        );
        peers[0].disconnect_policy(&app.manager);
        peers[0].acknowledge_policy(&app.manager, 1, false);
        assert_eq!(
            for_session(&app, sessions[0]).unwrap().phase,
            PolicyPhase::Failed(RdpClipboardPolicyFailure::HelperDisconnected)
        );

        let stale = from_status(
            false,
            RdpClipboardPolicyStatus::Confirmed {
                epoch: 9,
                enabled: true,
            },
        );
        assert_eq!((stale.enabled, stale.phase), (false, PolicyPhase::Pending));
        assert_eq!(app.manager.sessions().len(), 2);
    }

    #[test]
    fn unavailable_channel_cannot_be_promoted_by_a_late_ack() {
        let (app, sessions, peers) = fixture();
        peers[0].prepare_policy(&app.manager).unwrap();
        peers[0].policy_channel_unavailable(&app.manager);
        peers[0].acknowledge_policy(&app.manager, 0, true);
        peers[0].policy_channel_ready(&app.manager, 1);
        assert_eq!(
            for_session(&app, sessions[0]).unwrap().phase,
            PolicyPhase::Failed(RdpClipboardPolicyFailure::ChannelNotNegotiated)
        );
    }

    #[test]
    fn no_session_has_no_fabricated_policy_status() {
        let mut app = AditApp {
            manager: SessionManager::with_profiles(Vec::new()),
            main_view: MainView::Terminal,
            ..AditApp::default()
        };
        assert_eq!(for_selected_session(&app), None);
        assert_eq!(for_session(&app, SessionId::new()), None);
        request(&mut app, false);
        assert!(!app.rdp_clipboard);
        assert_eq!(for_selected_session(&app), None);
        assert_eq!(app.notice, "已请求关闭远程桌面剪贴板共享");
        request(&mut app, true);
        assert!(app.rdp_clipboard);
        assert_eq!(for_selected_session(&app), None);
        assert_eq!(app.notice, "已请求开启远程桌面剪贴板共享");
    }
}
