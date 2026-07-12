//! RDSTLS security (MS-RDPBCGR) — the authentication exchange GNOME Remote
//! Desktop's system-mode handover uses on the redirected reconnect, with the
//! one-time credentials from the Server Redirection PDU. IronRDP has only the
//! protocol flag, not the exchange, so we implement it here, ported from
//! FreeRDP's `rdstls.c`. It runs on the TLS stream after the X.224 negotiation
//! and before the MCS connect (i.e. where CredSSP would otherwise go).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const RDSTLS_VERSION_1: u16 = 0x0001;
const RDSTLS_TYPE_CAPABILITIES: u16 = 0x0001;
const RDSTLS_TYPE_AUTHREQ: u16 = 0x0002;
const RDSTLS_TYPE_AUTHRSP: u16 = 0x0004;
const RDSTLS_DATA_CAPABILITIES: u16 = 0x0001;
const RDSTLS_DATA_PASSWORD_CREDS: u16 = 0x0001;
const RDSTLS_DATA_RESULT_CODE: u16 = 0x0001;
const RDSTLS_RESULT_SUCCESS: u32 = 0x0000_0000;

/// One-time credentials from the Server Redirection PDU, for the RDSTLS auth.
pub(crate) struct RdstlsCreds {
    pub redirection_guid: Vec<u8>,
    pub username: String,
    pub domain: String,
    pub password: Vec<u8>,
}

/// Drive the client side of the RDSTLS authentication on `stream` (already
/// TLS-upgraded): receive Capabilities, send the password Auth Request, receive
/// the Auth Response. `Ok` only on `RDSTLS_RESULT_SUCCESS`.
pub(crate) async fn authenticate<S>(stream: &mut S, creds: &RdstlsCreds) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Receive the server's Capabilities PDU:
    //    version(2) type(2) dataType(2) supportedVersions(2)
    let mut caps = [0u8; 8];
    stream.read_exact(&mut caps).await?;
    if u16::from_le_bytes([caps[0], caps[1]]) != RDSTLS_VERSION_1
        || u16::from_le_bytes([caps[2], caps[3]]) != RDSTLS_TYPE_CAPABILITIES
        || u16::from_le_bytes([caps[4], caps[5]]) != RDSTLS_DATA_CAPABILITIES
        || u16::from_le_bytes([caps[6], caps[7]]) & RDSTLS_VERSION_1 == 0
    {
        return Err(err("unexpected RDSTLS capabilities PDU"));
    }

    // 2. Send the password Auth Request PDU.
    stream.write_all(&encode_auth_request(creds)).await?;
    stream.flush().await?;

    // 3. Receive the Auth Response PDU:
    //    version(2) type(2) dataType(2) resultCode(4)
    let mut rsp = [0u8; 10];
    stream.read_exact(&mut rsp).await?;
    if u16::from_le_bytes([rsp[0], rsp[1]]) != RDSTLS_VERSION_1
        || u16::from_le_bytes([rsp[2], rsp[3]]) != RDSTLS_TYPE_AUTHRSP
        || u16::from_le_bytes([rsp[4], rsp[5]]) != RDSTLS_DATA_RESULT_CODE
    {
        return Err(err("unexpected RDSTLS auth response PDU"));
    }
    let result = u32::from_le_bytes([rsp[6], rsp[7], rsp[8], rsp[9]]);
    if result != RDSTLS_RESULT_SUCCESS {
        return Err(err(&format!("RDSTLS auth rejected (result 0x{result:08x})")));
    }
    Ok(())
}

fn encode_auth_request(creds: &RdstlsCreds) -> Vec<u8> {
    let mut b = Vec::with_capacity(96);
    b.extend_from_slice(&RDSTLS_VERSION_1.to_le_bytes());
    b.extend_from_slice(&RDSTLS_TYPE_AUTHREQ.to_le_bytes());
    b.extend_from_slice(&RDSTLS_DATA_PASSWORD_CREDS.to_le_bytes());
    write_data(&mut b, &creds.redirection_guid);
    write_string(&mut b, &creds.username);
    write_string(&mut b, &creds.domain);
    write_data(&mut b, &creds.password);
    b
}

/// A data field: u16 byte-length + the raw bytes.
fn write_data(b: &mut Vec<u8>, data: &[u8]) {
    let len = data.len().min(u16::MAX as usize);
    b.extend_from_slice(&(len as u16).to_le_bytes());
    b.extend_from_slice(&data[..len]);
}

/// A string field: u16 byte-length (incl. the NUL) + UTF-16LE + NUL terminator.
/// An empty string is length 2 with a single NUL unit (matches FreeRDP).
fn write_string(b: &mut Vec<u8>, s: &str) {
    let mut units: Vec<u16> = s.encode_utf16().collect();
    units.push(0); // NUL terminator
    let byte_len = (units.len() * 2).min(u16::MAX as usize);
    b.extend_from_slice(&(byte_len as u16).to_le_bytes());
    for unit in units {
        b.extend_from_slice(&unit.to_le_bytes());
    }
}

fn err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_owned())
}
