//! Extended Clipboard text only. Bounds apply before allocation and inflation.
use crate::VncError;
use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
const TEXT: u32 = 1;
const CAPS: u32 = 1 << 24;
const REQUEST: u32 = 1 << 25;
const PEEK: u32 = 1 << 26;
const NOTIFY: u32 = 1 << 27;
const PROVIDE: u32 = 1 << 28;
const ACTIONS: u32 = CAPS | REQUEST | PEEK | NOTIFY | PROVIDE;
const MAX: usize = crate::limits::MAX_TEXT;
#[derive(Default)]
pub(crate) struct Clipboard {
    pub enabled: AtomicBool,
    permissions: Arc<Permissions>,
    pub fence: AtomicBool,
    local: Mutex<Option<String>>,
    remote_actions: Mutex<u32>,
    pub sender: Mutex<Option<tokio::sync::mpsc::Sender<super::client::messages::ClientMsg>>>,
}
/// Epoch invalidates queued offers when a user disables then re-enables sharing.
#[derive(Debug)]
pub(crate) struct Permissions(AtomicU64);
impl Default for Permissions {
    fn default() -> Self {
        Self(AtomicU64::new(3))
    }
}
impl Permissions {
    fn set(&self, send: bool, receive: bool) {
        let bits = u64::from(send) | (u64::from(receive) << 1);
        let _ = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                if old & 3 == bits {
                    None
                } else {
                    Some(((old >> 2).wrapping_add(1) << 2) | bits)
                }
            });
    }
    pub(crate) fn snapshot(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
    pub(crate) fn permits(&self, epoch: u64, message: &super::client::messages::ClientMsg) -> bool {
        use super::client::messages::ClientMsg;
        let value = self.snapshot();
        if matches!(message, ClientMsg::ExtendedCutText(bytes) if bytes.len() >= 4 && u32::from_be_bytes(bytes[..4].try_into().unwrap()) & CAPS != 0)
        {
            return true;
        }
        if value != epoch {
            return false;
        }
        match message {
            ClientMsg::LegacyCutText(_) => value & 1 != 0,
            ClientMsg::ExtendedCutText(bytes) if bytes.len() >= 4 => {
                match u32::from_be_bytes(bytes[..4].try_into().unwrap()) & ACTIONS {
                    PROVIDE | NOTIFY => value & 1 != 0,
                    REQUEST => value & 2 != 0,
                    CAPS => true,
                    _ => false,
                }
            }
            _ => false,
        }
    }
}
fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}
fn extended(flags: u32, data: &[u8]) -> Vec<u8> {
    let mut bytes = flags.to_be_bytes().to_vec();
    bytes.extend_from_slice(data);
    bytes
}
impl Clipboard {
    pub fn set_permissions(&self, send: bool, receive: bool) {
        self.permissions.set(send, receive);
        if !send {
            self.local.lock().unwrap().take();
        }
    }
    fn permissions(&self) -> (bool, bool) {
        let value = self.permissions.snapshot();
        (value & 1 != 0, value & 2 != 0)
    }
    pub fn copy(&self, text: String) -> Result<super::client::messages::ClientMsg, VncError> {
        let epoch = self.permissions.snapshot();
        self.copy_payload(text)
            .map(|message| self.guarded(message, epoch))
    }
    fn guarded(
        &self,
        message: super::client::messages::ClientMsg,
        epoch: u64,
    ) -> super::client::messages::ClientMsg {
        super::client::messages::ClientMsg::GuardedClipboard(
            Box::new(message),
            Arc::clone(&self.permissions),
            epoch,
        )
    }
    fn copy_payload(&self, text: String) -> Result<super::client::messages::ClientMsg, VncError> {
        if !self.permissions().0 {
            return Err(VncError::ClipboardUnsupported);
        }
        if text.len() > MAX || text.contains('\0') {
            return Err(VncError::ClipboardUnsupported);
        }
        let text = normalize(&text);
        if self.enabled.load(Ordering::Acquire) {
            {
                let mut local = self.local.lock().unwrap();
                if !self.permissions().0 {
                    return Err(VncError::ClipboardUnsupported);
                }
                *local = Some(text);
            }
            let actions = *self.remote_actions.lock().unwrap();
            if actions & NOTIFY != 0 {
                Ok(super::client::messages::ClientMsg::ExtendedCutText(
                    extended(NOTIFY | TEXT, &[]),
                ))
            } else if actions & PROVIDE != 0 {
                Ok(super::client::messages::ClientMsg::ExtendedCutText(
                    self.provide()?,
                ))
            } else {
                Err(VncError::ClipboardUnsupported)
            }
        } else {
            let bytes: Result<Vec<u8>, _> = text.chars().map(|c| u8::try_from(c as u32)).collect();
            Ok(super::client::messages::ClientMsg::LegacyCutText(
                bytes.map_err(|_| VncError::ClipboardUnsupported)?,
            ))
        }
    }
    fn provide(&self) -> Result<Vec<u8>, VncError> {
        let text = self.local.lock().unwrap().clone().unwrap_or_default();
        let mut utf8 = text.replace('\n', "\r\n").into_bytes();
        utf8.push(0);
        if utf8.len() > MAX {
            return Err(VncError::ClipboardUnsupported);
        }
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&(utf8.len() as u32).to_be_bytes())?;
        encoder.write_all(&utf8)?;
        Ok(extended(PROVIDE | TEXT, &encoder.finish()?))
    }
    pub async fn receive(
        &self,
        payload: &[u8],
    ) -> Result<(Option<String>, Option<bool>), VncError> {
        if payload.len() < 4 || payload.len() > MAX {
            return Err(VncError::InvalidImageData);
        }
        let epoch = self.permissions.snapshot();
        let flags = u32::from_be_bytes(payload[..4].try_into().unwrap());
        let mut reply = None;
        let mut capability = None;
        let mut text = None;
        if flags & CAPS != 0 {
            let formats = (flags & 0xffff).count_ones() as usize;
            if payload.len() != 4 + 4 * formats || flags & 0xe0ff0000 != 0 {
                return Err(VncError::InvalidImageData);
            }
            let supported = flags & TEXT != 0 && flags & PROVIDE != 0;
            self.enabled.store(supported, Ordering::Release);
            *self.remote_actions.lock().unwrap() = flags & ACTIONS;
            capability = Some(supported);
            if supported {
                // No unsolicited data: receive notification then request bounded text.
                reply = Some(extended(
                    CAPS | TEXT | REQUEST | PEEK | NOTIFY | PROVIDE,
                    &0_u32.to_be_bytes(),
                ));
            }
        } else {
            if !self.enabled.load(Ordering::Acquire)
                || (flags & ACTIONS).count_ones() != 1
                || flags & 0xe0ff0000 != 0
            {
                return Err(VncError::InvalidImageData);
            }
            match flags & ACTIONS {
                NOTIFY if payload.len() == 4 => {
                    if flags & TEXT != 0 && self.permissions().1 {
                        reply = Some(extended(REQUEST | TEXT, &[]));
                    } else if flags & TEXT == 0 && self.permissions().1 {
                        text = Some(String::new());
                    }
                }
                REQUEST if payload.len() == 4 && flags & TEXT != 0 && self.permissions().0 => {
                    reply = Some(self.provide()?);
                }
                REQUEST if payload.len() == 4 && flags & TEXT != 0 => {}
                PEEK if payload.len() == 4 && self.permissions().0 => {
                    let available = self.local.lock().unwrap().is_some();
                    reply = Some(extended(NOTIFY | if available { TEXT } else { 0 }, &[]));
                }
                PEEK if payload.len() == 4 => {}
                PROVIDE => {
                    if flags & 0xffff != TEXT {
                        return Err(VncError::InvalidImageData);
                    }
                    let decoder = flate2::read::ZlibDecoder::new(&payload[4..]);
                    let mut bounded = decoder.take((MAX + 5) as u64);
                    let mut bytes = Vec::new();
                    bounded.read_to_end(&mut bytes)?;
                    if bytes.len() < 5 || bytes.len() > MAX + 4 {
                        return Err(VncError::InvalidImageData);
                    }
                    let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                    if len == 0
                        || len > MAX
                        || bytes.len() != len + 4
                        || bytes.last() != Some(&0)
                        || bytes[4..bytes.len() - 1].contains(&0)
                    {
                        return Err(VncError::InvalidImageData);
                    }
                    let decoded = std::str::from_utf8(&bytes[4..bytes.len() - 1])
                        .map_err(|_| VncError::InvalidImageData)?;
                    if self.permissions().1 {
                        text = Some(normalize(decoded));
                    }
                }
                _ => return Err(VncError::InvalidImageData),
            }
        }
        let sender = self.sender.lock().unwrap().clone();
        if let (Some(reply), Some(sender)) = (reply, sender) {
            let action = u32::from_be_bytes(reply[..4].try_into().unwrap()) & ACTIONS;
            if matches!(action, PROVIDE | NOTIFY) && !self.permissions().0 {
                return Ok((text, capability));
            }
            sender
                .send(self.guarded(
                    super::client::messages::ClientMsg::ExtendedCutText(reply),
                    epoch,
                ))
                .await?;
        }
        Ok((text, capability))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::messages::ClientMsg;
    fn payload(message: ClientMsg) -> ClientMsg {
        match message {
            ClientMsg::GuardedClipboard(inner, _, _) => *inner,
            other => other,
        }
    }
    async fn received(rx: &mut tokio::sync::mpsc::Receiver<ClientMsg>) -> Option<ClientMsg> {
        rx.recv().await.map(payload)
    }
    async fn negotiated() -> (Clipboard, tokio::sync::mpsc::Receiver<ClientMsg>) {
        let clip = Clipboard::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        *clip.sender.lock().unwrap() = Some(tx);
        assert_eq!(
            clip.receive(&extended(
                CAPS | TEXT | REQUEST | PEEK | NOTIFY | PROVIDE,
                &0_u32.to_be_bytes()
            ))
            .await
            .unwrap(),
            (None, Some(true))
        );
        assert!(matches!(
            received(&mut rx).await,
            Some(ClientMsg::ExtendedCutText(_))
        ));
        (clip, rx)
    }
    #[test]
    fn legacy_uses_latin1_and_refuses_unrepresentable_or_unbounded_text() {
        let clip = Clipboard::default();
        assert!(
            matches!(payload(clip.copy("café\r\n".into()).unwrap()), ClientMsg::LegacyCutText(bytes) if bytes == b"caf\xe9\n")
        );
        assert!(matches!(
            clip.copy("中文".into()),
            Err(VncError::ClipboardUnsupported)
        ));
        assert!(clip.copy("a".repeat(MAX + 1)).is_err());
        assert!(clip.copy("a\0b".into()).is_err());
    }
    #[tokio::test]
    async fn extended_roundtrips_unicode_multiline_empty_and_has_no_cross_session_cache() {
        let (clip, mut rx) = negotiated().await;
        for text in ["中文😀\r\n第二行", "", "é\nlast"] {
            assert!(
                matches!(payload(clip.copy(text.into()).unwrap()), ClientMsg::ExtendedCutText(bytes) if bytes == extended(NOTIFY | TEXT, &[]))
            );
            clip.receive(&extended(REQUEST | TEXT, &[])).await.unwrap();
            let Some(ClientMsg::ExtendedCutText(bytes)) = received(&mut rx).await else {
                panic!("missing provide")
            };
            assert_eq!(
                clip.receive(&bytes).await.unwrap().0.as_deref(),
                Some(normalize(text).as_str())
            );
        }
        let (other, mut other_rx) = negotiated().await;
        other.receive(&extended(PEEK, &[])).await.unwrap();
        assert!(
            matches!(received(&mut other_rx).await, Some(ClientMsg::ExtendedCutText(bytes)) if bytes == extended(NOTIFY, &[]))
        );
    }
    #[tokio::test]
    async fn readonly_clears_ownership_and_never_answers_text_requests() {
        let (clip, mut rx) = negotiated().await;
        clip.copy("cached local text".into()).unwrap();
        clip.set_permissions(false, true);
        assert!(clip.local.lock().unwrap().is_none());
        assert!(clip.copy("blocked".into()).is_err());
        clip.receive(&extended(REQUEST | TEXT, &[])).await.unwrap();
        clip.receive(&extended(PEEK, &[])).await.unwrap();
        assert!(rx.try_recv().is_err());
        clip.receive(&extended(NOTIFY | TEXT, &[])).await.unwrap();
        assert!(
            matches!(received(&mut rx).await, Some(ClientMsg::ExtendedCutText(bytes)) if bytes == extended(REQUEST | TEXT, &[]))
        );
        clip.set_permissions(true, false);
        clip.receive(&extended(NOTIFY | TEXT, &[])).await.unwrap();
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn malformed_caps_actions_utf8_and_inflate_bombs_are_rejected() {
        let (clip, _) = negotiated().await;
        assert!(clip.receive(&extended(CAPS | TEXT, &[])).await.is_err());
        assert!(clip
            .receive(&extended(NOTIFY | REQUEST | TEXT, &[]))
            .await
            .is_err());
        assert!(clip
            .receive(&extended(PROVIDE | 2, &[1, 2, 3]))
            .await
            .is_err());
        for bytes in [
            vec![0, 0, 0, 2, 0xff, 0],
            vec![0, 0, 0, 3, b'a', 0],
            vec![0, 0, 0, 2, b'a', b'b'],
            vec![0, 0, 0, 3, 0, b'a', 0],
            vec![0; MAX + 6],
        ] {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&bytes).unwrap();
            assert!(clip
                .receive(&extended(PROVIDE | TEXT, &encoder.finish().unwrap()))
                .await
                .is_err());
        }
    }
    #[tokio::test]
    async fn disabling_sharing_invalidates_previously_queued_clipboard_packets() {
        let (clip, mut rx) = negotiated().await;
        clip.copy("previously offered secret".into()).unwrap();
        clip.receive(&extended(REQUEST | TEXT, &[])).await.unwrap();
        let queued = rx.recv().await.unwrap();
        clip.set_permissions(false, true);
        clip.set_permissions(true, true);
        let (mut writer, mut reader) = tokio::io::duplex(128);
        queued.write(&mut writer).await.unwrap();
        drop(writer);
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(
            bytes.is_empty(),
            "old clipboard body crossed a sharing-policy change"
        );
    }
}
