//! MS-RDPEDISP over the existing drdynvc channel; no second transport.
use std::sync::{Arc, Mutex};

use adit_rdp_proto::{monitor_bounds, DisplayControlState, Monitor};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::{
    DeviceScaleFactor, DisplayControlCapabilities, DisplayControlMonitorLayout, DisplayControlPdu,
    MonitorLayoutEntry, MonitorOrientation,
};
use ironrdp_dvc::encode_dvc_messages;
use ironrdp_session::ActiveStage;
use ironrdp_svc::ChannelFlags;
use tokio::time::{Duration, Instant};

pub(crate) type SharedDisplay = Arc<Mutex<DisplayControlState>>;

/// The server opens the dynamic channel asynchronously. Do not retain an
/// unusable request forever if it never opens or never advertises capabilities.
pub(crate) const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct PendingLayout {
    pub(crate) request_id: u64,
    pub(crate) monitors: Vec<Monitor>,
    pub(crate) deadline: Option<Instant>,
}

impl PendingLayout {
    pub(crate) fn new(request_id: u64, monitors: Vec<Monitor>) -> Self {
        Self {
            request_id,
            monitors,
            deadline: None,
        }
    }

    pub(crate) fn start_waiting(&mut self, now: Instant) {
        self.deadline = Some(now + CAPABILITIES_TIMEOUT);
    }
}

pub(crate) fn cancel_pending(pending: &mut Option<PendingLayout>, request_id: u64) {
    if pending
        .as_ref()
        .is_some_and(|request| request.request_id == request_id)
    {
        *pending = None;
    }
}

pub(crate) fn poll_layout(
    request: &PendingLayout,
    now: Instant,
    channel_closed: bool,
    encode: impl FnOnce(&[Monitor]) -> Option<Result<Vec<u8>, String>>,
) -> Option<Result<Vec<u8>, String>> {
    if channel_closed {
        Some(Err("DisplayControl channel closed".to_owned()))
    } else if request.deadline.is_some_and(|deadline| now >= deadline) {
        Some(Err("DisplayControl capabilities timed out".to_owned()))
    } else {
        encode(&request.monitors)
    }
}

/// Detect closing even when the channel had opened but never advertised caps.
#[derive(Default)]
pub(crate) struct ChannelWatch(bool);

impl ChannelWatch {
    pub(crate) fn observe(&mut self, open: bool) -> bool {
        let closed = self.0 && !open;
        self.0 = open;
        closed
    }
}

pub(crate) fn channel_open(stage: &mut ActiveStage) -> bool {
    stage
        .get_dvc::<DisplayControlClient>()
        .is_some_and(|channel| channel.channel_id().is_some())
}

pub(crate) fn ready(stage: &mut ActiveStage, state: &SharedDisplay) -> bool {
    let has_caps = state.lock().unwrap_or_else(|e| e.into_inner()).max_monitors > 0;
    has_caps
        && stage
            .get_dvc::<DisplayControlClient>()
            .is_some_and(|channel| {
                channel.channel_id().is_some()
                    && channel
                        .channel_processor_downcast_ref::<DisplayControlClient>()
                        .is_some_and(DisplayControlClient::ready)
            })
}

pub(crate) fn capabilities(state: &SharedDisplay, caps: &DisplayControlCapabilities) {
    // 0.8 exposes area but not MaxNumMonitors. Read the public encoder's first
    // u32 rather than guessing a count or patching another upstream crate.
    if let Ok(bytes) = ironrdp_core::encode_vec(caps) {
        if let Ok(count) = <[u8; 4]>::try_from(&bytes[..4]) {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.max_monitors = u32::from_le_bytes(count);
            state.max_area = caps.max_monitor_area();
        }
    }
}

fn layout(monitors: &[Monitor]) -> Result<DisplayControlMonitorLayout, String> {
    monitor_bounds(monitors).map_err(str::to_owned)?;
    let entries = monitors
        .iter()
        .map(|m| {
            let entry = if m.primary {
                MonitorLayoutEntry::new_primary(u32::from(m.width), u32::from(m.height))
            } else {
                MonitorLayoutEntry::new_secondary(u32::from(m.width), u32::from(m.height))
            }
            .map_err(|e| e.to_string())?;
            entry
                .with_orientation(if m.width >= m.height {
                    MonitorOrientation::Landscape
                } else {
                    MonitorOrientation::Portrait
                })
                .with_device_scale_factor(DeviceScaleFactor::Scale100Percent)
                .with_position(m.left, m.top)
                .and_then(|e| e.with_desktop_scale_factor(m.scale_percent))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    DisplayControlMonitorLayout::new(&entries).map_err(|e| e.to_string())
}

/// None means capabilities/channel are not ready, so the caller must retain the request.
pub(crate) fn encode_layout(
    stage: &mut ActiveStage,
    monitors: &[Monitor],
    state: &SharedDisplay,
) -> Option<Result<Vec<u8>, String>> {
    let caps = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if caps.max_monitors == 0 {
        return None;
    }
    let result = (|| {
        let pdu = layout(monitors)?;
        let area: u64 = monitors
            .iter()
            .map(|m| u64::from(m.width) * u64::from(m.height))
            .sum();
        if monitors.len() as u32 > caps.max_monitors || area > caps.max_area {
            return Err("requested layout exceeds server monitor count/area capability".to_owned());
        }
        let channel = stage
            .get_dvc::<DisplayControlClient>()
            .ok_or_else(|| "DisplayControl channel disappeared".to_owned())?;
        let client = channel
            .channel_processor_downcast_ref::<DisplayControlClient>()
            .ok_or_else(|| "DisplayControl processor unavailable".to_owned())?;
        if !client.ready() {
            return Err("DisplayControl capabilities not ready".to_owned());
        }
        let id = channel
            .channel_id()
            .ok_or_else(|| "DisplayControl channel not connected".to_owned())?;
        let messages = encode_dvc_messages(
            id,
            vec![Box::new(DisplayControlPdu::from(pdu))],
            ChannelFlags::empty(),
        )
        .map_err(|e| e.to_string())?;
        stage
            .encode_dvc_messages(messages)
            .map_err(|e| e.to_string())
    })();
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::{Decode as _, ReadCursor};

    fn monitor(left: i32, primary: bool) -> Monitor {
        Monitor {
            left,
            top: 0,
            width: 800,
            height: 600,
            primary,
            scale_percent: 150,
        }
    }

    #[test]
    fn complete_layout_encodes_positions_primary_and_scale() {
        let expected = layout(&[monitor(-800, false), monitor(0, true)]).unwrap();
        let bytes = ironrdp_core::encode_vec(&expected).unwrap();
        let actual = DisplayControlMonitorLayout::decode(&mut ReadCursor::new(&bytes)).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual.monitors()[0].position(), Some((-800, 0)));
        assert!(actual.monitors()[1].is_primary());
        assert_eq!(actual.monitors()[0].desktop_scale_factor(), Some(150));
        assert_eq!(
            actual.monitors()[0].orientation(),
            Some(MonitorOrientation::Landscape)
        );
    }

    #[test]
    fn caps_publish_real_server_limits() {
        let state = SharedDisplay::default();
        capabilities(
            &state,
            &DisplayControlCapabilities::new(2, 4096, 2048).unwrap(),
        );
        let caps = state.lock().unwrap();
        assert_eq!(caps.max_monitors, 2);
        assert_eq!(caps.max_area, 2 * 4096 * 2048);
    }

    #[test]
    fn a_missing_channel_never_encodes_resize_and_reports_layout_failure() {
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: Default::default(),
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        let state = SharedDisplay::default();
        let monitors = vec![monitor(0, true)];
        assert!(!ready(&mut stage, &state));
        assert!(encode_layout(&mut stage, &monitors, &state).is_none());
        capabilities(
            &state,
            &DisplayControlCapabilities::new(2, 4096, 2048).unwrap(),
        );
        assert!(!ready(&mut stage, &state));
        assert_eq!(
            encode_layout(&mut stage, &monitors, &state),
            Some(Err("DisplayControl channel disappeared".to_owned()))
        );
    }

    #[test]
    fn parked_layout_times_out_or_sends_when_capabilities_arrive() {
        let now = Instant::now();
        let mut request = PendingLayout::new(7, vec![monitor(0, true)]);
        request.start_waiting(now);
        assert!(poll_layout(&request, now, false, |_| None).is_none());
        assert_eq!(
            poll_layout(&request, now + Duration::from_secs(4), false, |_| Some(Ok(
                vec![7]
            ))),
            Some(Ok(vec![7]))
        );
        assert_eq!(
            poll_layout(&request, now + CAPABILITIES_TIMEOUT, false, |_| panic!(
                "expired requests must not be encoded"
            )),
            Some(Err("DisplayControl capabilities timed out".to_owned()))
        );
        assert_eq!(
            poll_layout(&request, now, false, |_| Some(Err(
                "encoding failed".to_owned()
            ))),
            Some(Err("encoding failed".to_owned()))
        );
    }

    #[test]
    fn closing_before_capabilities_and_stale_cancellation_are_distinct() {
        let now = Instant::now();
        let mut channels = ChannelWatch::default();
        assert!(!channels.observe(false));
        assert!(!channels.observe(true));
        assert!(channels.observe(false));
        assert!(!channels.observe(false));
        let mut request = PendingLayout::new(8, vec![monitor(0, true)]);
        request.start_waiting(now);
        assert_eq!(
            poll_layout(&request, now, true, |_| panic!(
                "closed channel must not encode"
            )),
            Some(Err("DisplayControl channel closed".to_owned()))
        );
        let mut pending = Some(request);
        cancel_pending(&mut pending, 7);
        assert_eq!(pending.as_ref().unwrap().request_id, 8);
        cancel_pending(&mut pending, 8);
        assert!(pending.is_none());
        let mut retry = PendingLayout::new(9, vec![monitor(0, true)]);
        retry.start_waiting(now);
        assert_eq!(
            poll_layout(&retry, now, false, |_| Some(Ok(vec![9]))),
            Some(Ok(vec![9]))
        );
    }
}
