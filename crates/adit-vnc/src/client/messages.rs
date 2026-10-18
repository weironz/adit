use crate::{PixelFormat, Rect, VncEncoding, VncError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub(crate) enum ClientMsg {
    Flush(tokio::sync::oneshot::Sender<()>),
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<VncEncoding>),
    FramebufferUpdateRequest(Rect, u8),
    KeyEvent(u32, bool),
    Fence(u32, Vec<u8>),
    PointerEvent(u16, u16, u8),
    GuardedClipboard(
        Box<ClientMsg>,
        std::sync::Arc<crate::clipboard::Permissions>,
        u64,
    ),
    LegacyCutText(Vec<u8>),
    TightCutText(Vec<u8>),
    ExtendedCutText(Vec<u8>),
    #[cfg(not(target_arch = "wasm32"))]
    SetDesktopSize(crate::DesktopLayout, std::sync::Arc<crate::flow::Flow>),
}

impl ClientMsg {
    pub(crate) async fn write<S>(self, writer: &mut S) -> Result<(), VncError>
    where
        S: AsyncWrite + Unpin,
    {
        match self {
            ClientMsg::Flush(done) => {
                writer.flush().await?;
                let _ = done.send(());
                Ok(())
            }
            ClientMsg::GuardedClipboard(message, permissions, epoch) => {
                if permissions.permits(epoch, &message) {
                    Box::pin(message.write(writer)).await?;
                }
                Ok(())
            }
            ClientMsg::SetPixelFormat(pf) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [0]       | message-type |
                // | 3            |              | padding      |
                // | 16           | PIXEL_FORMAT | pixel-format |
                // +--------------+--------------+--------------+
                let mut payload = vec![0_u8, 0, 0, 0];
                payload.extend(<PixelFormat as Into<Vec<u8>>>::into(pf));
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::SetEncodings(encodings) => {
                //  +--------------+--------------+---------------------+
                // | No. of bytes | Type [Value] | Description         |
                // +--------------+--------------+---------------------+
                // | 1            | U8 [2]       | message-type        |
                // | 1            |              | padding             |
                // | 2            | U16          | number-of-encodings |
                // +--------------+--------------+---------------------+

                // This is followed by number-of-encodings repetitions of the following:
                // +--------------+--------------+---------------+
                // | No. of bytes | Type [Value] | Description   |
                // +--------------+--------------+---------------+
                // | 4            | S32          | encoding-type |
                // +--------------+--------------+---------------+
                let mut payload = vec![2, 0];
                payload.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
                for e in encodings {
                    payload.write_u32(e.into()).await?;
                }
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::FramebufferUpdateRequest(rect, incremental) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [3]       | message-type |
                // | 1            | U8           | incremental  |
                // | 2            | U16          | x-position   |
                // | 2            | U16          | y-position   |
                // | 2            | U16          | width        |
                // | 2            | U16          | height       |
                // +--------------+--------------+--------------+
                let mut payload = vec![3, incremental];
                payload.extend_from_slice(&rect.x.to_be_bytes());
                payload.extend_from_slice(&rect.y.to_be_bytes());
                payload.extend_from_slice(&rect.width.to_be_bytes());
                payload.extend_from_slice(&rect.height.to_be_bytes());
                writer.write_all(&payload).await?;
                #[cfg(debug_assertions)]
                if std::env::var_os("ADIT_VNC_TRACE_FLOW").is_some() {
                    eprintln!(
                        "vnc-flow request incremental={incremental} rect={},{},{}x{}",
                        rect.x, rect.y, rect.width, rect.height
                    );
                }
                Ok(())
            }
            ClientMsg::Fence(flags, payload) => {
                if payload.len() > 64 {
                    return Err(VncError::InvalidImageData);
                }
                writer.write_all(&[248, 0, 0, 0]).await?;
                writer.write_u32(flags).await?;
                writer.write_u8(payload.len() as u8).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::KeyEvent(keycode, down) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [4]       | message-type |
                // | 1            | U8           | down-flag    |
                // | 2            |              | padding      |
                // | 4            | U32          | key          |
                // +--------------+--------------+--------------+
                let mut payload = vec![4, down as u8, 0, 0];
                payload.write_u32(keycode).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::PointerEvent(x, y, mask) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [5]       | message-type |
                // | 1            | U8           | button-mask  |
                // | 2            | U16          | x-position   |
                // | 2            | U16          | y-position   |
                // +--------------+--------------+--------------+
                let mut payload = vec![5, mask];
                payload.write_u16(x).await?;
                payload.write_u16(y).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            #[cfg(not(target_arch = "wasm32"))]
            ClientMsg::SetDesktopSize(layout, flow) => {
                layout.validate()?;
                let mut payload = vec![251, 0];
                payload.extend_from_slice(&layout.width.to_be_bytes());
                payload.extend_from_slice(&layout.height.to_be_bytes());
                payload.extend_from_slice(&[layout.screens.len() as u8, 0]);
                for screen in &layout.screens {
                    payload.extend_from_slice(&screen.id.to_be_bytes());
                    payload.extend_from_slice(&screen.x.to_be_bytes());
                    payload.extend_from_slice(&screen.y.to_be_bytes());
                    payload.extend_from_slice(&screen.width.to_be_bytes());
                    payload.extend_from_slice(&screen.height.to_be_bytes());
                    payload.extend_from_slice(&screen.flags.to_be_bytes());
                }
                // Some servers release resize confirmations only with an outstanding
                // update request. Keep it ordered with the resize and bounded to one pixel.
                if let Some(full) = flow.request_resize_refresh() {
                    if full {
                        payload.extend_from_slice(&[3, 0, 0, 0, 0, 0]);
                        payload.extend_from_slice(&layout.width.to_be_bytes());
                        payload.extend_from_slice(&layout.height.to_be_bytes());
                    } else {
                        payload.extend_from_slice(&[3, 1, 0, 0, 0, 0, 0, 1, 0, 1]);
                    }
                }
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::TightCutText(bytes) => {
                if bytes.len() > crate::limits::MAX_TEXT || bytes.contains(&0) {
                    return Err(VncError::ClipboardUnsupported);
                }
                writer.write_u32(crate::tight::CUT_TEXT).await?;
                writer.write_u32(bytes.len() as u32).await?;
                writer.write_all(&bytes).await?;
                Ok(())
            }
            ClientMsg::LegacyCutText(bytes) => {
                writer.write_all(&[6, 0, 0, 0]).await?;
                writer.write_i32(bytes.len() as i32).await?;
                writer.write_all(&bytes).await?;
                Ok(())
            }
            ClientMsg::ExtendedCutText(bytes) => {
                let length = -(bytes.len() as i32);
                writer.write_all(&[6, 0, 0, 0]).await?;
                writer.write_i32(length).await?;
                writer.write_all(&bytes).await?;
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum ServerMsg {
    FramebufferUpdate(u16),
    // SetColorMapEntries,
    Bell,
    Fence(u32, Vec<u8>),
    ServerCutText(String),
    TightCutText(String),
    ExtendedCutText(Vec<u8>),
}

impl ServerMsg {
    pub(super) async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let server_msg = reader.read_u8().await?;

        match server_msg {
            0 => {
                // FramebufferUpdate
                //   +--------------+--------------+----------------------+
                //   | No. of bytes | Type [Value] | Description          |
                //   +--------------+--------------+----------------------+
                //   | 1            | U8 [0]       | message-type         |
                //   | 1            |              | padding              |
                //   | 2            | U16          | number-of-rectangles |
                //   +--------------+--------------+----------------------+
                let _padding = reader.read_u8().await?;
                let rects = reader.read_u16().await?;
                Ok(ServerMsg::FramebufferUpdate(rects))
            }
            1 => {
                // SetColorMapEntries
                // +--------------+--------------+------------------+
                // | No. of bytes | Type [Value] | Description      |
                // +--------------+--------------+------------------+
                // | 1            | U8 [1]       | message-type     |
                // | 1            |              | padding          |
                // | 2            | U16          | first-color      |
                // | 2            | U16          | number-of-colors |
                // +--------------+--------------+------------------+
                Err(VncError::WrongServerMessage)
            }
            2 => {
                // Bell
                //   +--------------+--------------+--------------+
                //   | No. of bytes | Type [Value] | Description  |
                //   +--------------+--------------+--------------+
                //   | 1            | U8 [2]       | message-type |
                //   +--------------+--------------+--------------+
                Ok(ServerMsg::Bell)
            }
            252 => {
                let mut suffix = [0; 3];
                reader.read_exact(&mut suffix).await?;
                if suffix != [0, 2, 0] {
                    return Err(VncError::WrongServerMessage);
                }
                let length = reader.read_u32().await? as usize;
                if length > crate::limits::MAX_TEXT {
                    return Err(VncError::InvalidImageData);
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await?;
                if bytes.contains(&0) {
                    return Err(VncError::InvalidImageData);
                }
                let text = String::from_utf8(bytes).map_err(|_| VncError::InvalidImageData)?;
                Ok(Self::TightCutText(
                    text.replace("\r\n", "\n").replace('\r', "\n"),
                ))
            }
            248 => {
                let mut padding = [0; 3];
                reader.read_exact(&mut padding).await?;
                let flags = reader.read_u32().await?;
                let length = reader.read_u8().await? as usize;
                if length > 64 {
                    return Err(VncError::InvalidImageData);
                }
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).await?;
                Ok(Self::Fence(flags, payload))
            }
            3 => {
                // ServerCutText
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [3]       | message-type |
                // | 3            |              | padding      |
                // | 4            | U32          | length       |
                // | length       | U8 array     | text         |
                // +--------------+--------------+--------------+
                let mut padding = [0; 3];
                reader.read_exact(&mut padding).await?;
                let length = reader.read_i32().await?;
                let len = length.unsigned_abs() as usize;
                if (length < 0 && len < 4) || len > crate::limits::MAX_TEXT {
                    return Err(VncError::InvalidImageData);
                }
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).await?;
                if length < 0 {
                    Ok(Self::ExtendedCutText(bytes))
                } else {
                    Ok(Self::ServerCutText(
                        bytes.into_iter().map(char::from).collect(),
                    ))
                }
            }
            _ => Err(VncError::WrongServerMessage),
        }
    }
}
