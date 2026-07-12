//! RDP Server Redirection (MS-RDPBCGR 2.2.13) parsing.
//!
//! GNOME Remote Desktop's system ("Remote Login", headless) mode authenticates
//! the client on the main daemon, then sends a **Server Redirection** PDU handing
//! the client off to the actual session's RDP endpoint. IronRDP has no support
//! for this (its `ShareControlPdu` doesn't parse `ServerRedirect`, pduType 0xa),
//! so we detect and parse the packet ourselves and drive a reconnect.
//!
//! This module only depends on IronRDP's public MCS decoder; drop it and use
//! upstream if IronRDP grows redirection support. See `IRONRDP-PATCHES.md`.

use ironrdp_pdu::mcs::decode_send_data_indication;

// ── LB_* redirection field flags (MS-RDPBCGR 2.2.13.1) ──────────────────────────
const LB_TARGET_NET_ADDRESS: u32 = 0x0000_0001;
const LB_LOAD_BALANCE_INFO: u32 = 0x0000_0002;
const LB_USERNAME: u32 = 0x0000_0004;
const LB_DOMAIN: u32 = 0x0000_0008;
const LB_PASSWORD: u32 = 0x0000_0010;
const LB_TARGET_FQDN: u32 = 0x0000_0100;
const LB_TARGET_NETBIOS_NAME: u32 = 0x0000_0200;
const LB_TARGET_NET_ADDRESSES: u32 = 0x0000_0800;
const LB_CLIENT_TSV_URL: u32 = 0x0000_1000;
const LB_PASSWORD_IS_PK_ENCRYPTED: u32 = 0x0000_4000;
const LB_REDIRECTION_GUID: u32 = 0x0000_8000;
const LB_TARGET_CERTIFICATE: u32 = 0x0001_0000;

/// `SEC_REDIRECTION_PKT` — the redirection packet's leading `flags` value.
const SEC_REDIRECTION_PKT: u16 = 0x0400;

/// ShareControlPduType::ServerRedirect.
const PDU_TYPE_SERVER_REDIRECT: u16 = 0xa;

/// Everything we need to reconnect to the redirected endpoint. Some fields mirror
/// the wire packet for logging/debugging even when the reconnect doesn't consume
/// them directly.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub(crate) struct Redirection {
    pub redir_flags: u32,
    pub session_id: u32,
    /// Explicit target IP/host (LB_TARGET_NET_ADDRESS), if given.
    pub target_net_address: Option<String>,
    /// Target FQDN (LB_TARGET_FQDN), if given.
    pub target_fqdn: Option<String>,
    /// Routing token (LB_LOAD_BALANCE_INFO), e.g. `Cookie: msts=...\r\n`. Passed
    /// verbatim in the X.224 connection request on reconnect.
    pub load_balance_info: Option<Vec<u8>>,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// Password/cookie (LB_PASSWORD) — the one-time password bytes, verbatim, for
    /// the RDSTLS reconnect.
    pub password: Option<Vec<u8>>,
    /// Redirection GUID (LB_REDIRECTION_GUID) — echoed in the RDSTLS auth request.
    pub redirection_guid: Option<Vec<u8>>,
    /// Whether the password is flagged PK-encrypted (informational; we forward the
    /// bytes verbatim either way).
    pub password_is_pk_encrypted: bool,
}

impl Redirection {
    /// The host to reconnect to: the explicit net address, else the FQDN.
    pub(crate) fn host(&self) -> Option<&str> {
        self.target_net_address
            .as_deref()
            .or(self.target_fqdn.as_deref())
    }
}

/// If `payload` (a full TPKT/X.224/MCS frame on the I/O channel) is a Server
/// Redirection PDU, parse it. `None` for any other PDU.
pub(crate) fn detect(payload: &[u8], io_channel_id: u16) -> Option<Redirection> {
    let ctx = decode_send_data_indication(payload).ok()?;
    if ctx.channel_id != io_channel_id {
        return None;
    }
    let data = ctx.user_data;

    // Share Control Header: totalLength(2), pduType(2), pduSource(2).
    if data.len() < 6 {
        return None;
    }
    let pdu_type = u16::from_le_bytes([data[2], data[3]]) & 0x000f;
    if pdu_type != PDU_TYPE_SERVER_REDIRECT {
        return None;
    }

    // After the header comes a pad then the redirection packet whose first u16 is
    // SEC_REDIRECTION_PKT. The pad width varies by encoder, so locate the marker
    // in the next few bytes rather than hard-coding it.
    let start = (6..=10).find(|&off| {
        data.len() >= off + 2 && u16::from_le_bytes([data[off], data[off + 1]]) == SEC_REDIRECTION_PKT
    })?;

    parse_packet(&data[start..])
}

fn parse_packet(mut p: &[u8]) -> Option<Redirection> {
    // flags(2) length(2) sessionID(4) redirFlags(4)
    let _flags = read_u16(&mut p)?;
    let _length = read_u16(&mut p)?;
    let session_id = read_u32(&mut p)?;
    let redir_flags = read_u32(&mut p)?;

    let mut r = Redirection {
        redir_flags,
        session_id,
        password_is_pk_encrypted: redir_flags & LB_PASSWORD_IS_PK_ENCRYPTED != 0,
        ..Default::default()
    };

    // Fields follow in flag order (MS-RDPBCGR 2.2.13.1).
    if redir_flags & LB_TARGET_NET_ADDRESS != 0 {
        r.target_net_address = Some(read_string(&mut p)?);
    }
    if redir_flags & LB_LOAD_BALANCE_INFO != 0 {
        r.load_balance_info = Some(read_data(&mut p)?);
    }
    if redir_flags & LB_USERNAME != 0 {
        r.username = Some(read_string(&mut p)?);
    }
    if redir_flags & LB_DOMAIN != 0 {
        r.domain = Some(read_string(&mut p)?);
    }
    if redir_flags & LB_PASSWORD != 0 {
        r.password = Some(read_data(&mut p)?);
    }
    if redir_flags & LB_TARGET_FQDN != 0 {
        r.target_fqdn = Some(read_string(&mut p)?);
    }
    if redir_flags & LB_TARGET_NETBIOS_NAME != 0 {
        let _ = read_string(&mut p)?;
    }
    if redir_flags & LB_TARGET_NET_ADDRESSES != 0 {
        let _ = read_data(&mut p)?;
    }
    if redir_flags & LB_CLIENT_TSV_URL != 0 {
        let _ = read_data(&mut p)?;
    }
    if redir_flags & LB_REDIRECTION_GUID != 0 {
        r.redirection_guid = Some(read_data(&mut p)?);
    }
    if redir_flags & LB_TARGET_CERTIFICATE != 0 {
        let _ = read_data(&mut p)?;
    }

    Some(r)
}

// ── little-endian readers over a moving slice ──────────────────────────────────

fn read_u16(p: &mut &[u8]) -> Option<u16> {
    let (a, rest) = p.split_first_chunk::<2>()?;
    *p = rest;
    Some(u16::from_le_bytes(*a))
}

fn read_u32(p: &mut &[u8]) -> Option<u32> {
    let (a, rest) = p.split_first_chunk::<4>()?;
    *p = rest;
    Some(u32::from_le_bytes(*a))
}

/// A redirection string: u32 byte-length + UTF-16LE (incl. null terminator).
fn read_string(p: &mut &[u8]) -> Option<String> {
    let bytes = read_data(p)?;
    Some(decode_utf16le(&bytes))
}

/// A redirection data field: u32 byte-length + that many raw bytes.
fn read_data(p: &mut &[u8]) -> Option<Vec<u8>> {
    let len = read_u32(p)? as usize;
    if p.len() < len {
        return None;
    }
    let (data, rest) = p.split_at(len);
    *p = rest;
    Some(data.to_vec())
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}
