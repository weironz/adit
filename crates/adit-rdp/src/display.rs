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

pub(crate) type SharedDisplay = Arc<Mutex<DisplayControlState>>;

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
}
