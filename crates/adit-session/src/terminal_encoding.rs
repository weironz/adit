use adit_domain::TerminalEncoding;
use encoding_rs::{Decoder, Encoding};

pub(crate) struct TerminalCodec {
    encoding: TerminalEncoding,
    decoder: Option<Decoder>,
}

fn encoding(encoding: TerminalEncoding) -> &'static Encoding {
    match encoding {
        TerminalEncoding::Utf8 => encoding_rs::UTF_8,
        TerminalEncoding::Gbk => encoding_rs::GBK,
        TerminalEncoding::Big5 => encoding_rs::BIG5,
        TerminalEncoding::ShiftJis => encoding_rs::SHIFT_JIS,
        TerminalEncoding::Windows1252 => encoding_rs::WINDOWS_1252,
    }
}

impl TerminalCodec {
    pub(crate) fn new(selected: TerminalEncoding) -> Self {
        Self {
            encoding: selected,
            decoder: (selected != TerminalEncoding::Utf8)
                .then(|| encoding(selected).new_decoder_without_bom_handling()),
        }
    }

    pub(crate) fn decode(&mut self, bytes: &[u8]) -> Vec<u8> {
        let Some(decoder) = &mut self.decoder else {
            // The VT parser already owns UTF-8 state and byte-level control codes.
            return bytes.to_vec();
        };
        let mut text = String::with_capacity(bytes.len().saturating_mul(4).saturating_add(16));
        let (status, read, _) = decoder.decode_to_string(bytes, &mut text, false);
        debug_assert_eq!(read, bytes.len());
        debug_assert_eq!(status, encoding_rs::CoderResult::InputEmpty);
        text.into_bytes()
    }

    pub(crate) fn encode(&self, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        if self.encoding == TerminalEncoding::Utf8 || bytes.is_ascii() {
            return Ok(bytes);
        }
        // Mouse reporting and other terminal control packets can contain raw
        // non-UTF-8 bytes; these are protocol packets, not committed user text.
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(bytes);
        };
        let (encoded, _, unmappable) = encoding(self.encoding).encode(text);
        if unmappable {
            return Err(format!(
                "输入包含 {} 无法表示的字符，整段输入未发送",
                self.encoding
            ));
        }
        Ok(encoded.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_gbk_character_is_reassembled_without_changing_escape_sequences() {
        let mut codec = TerminalCodec::new(TerminalEncoding::Gbk);
        assert!(codec.decode(&[0xd6]).is_empty());
        assert_eq!(
            codec.decode(&[0xd0, 0x1b, b'[', b'0', b'm']),
            "中\u{1b}[0m".as_bytes()
        );
    }

    #[test]
    fn unrepresentable_commit_is_rejected_atomically() {
        let codec = TerminalCodec::new(TerminalEncoding::Gbk);
        assert_eq!(
            codec.encode("中文".as_bytes().to_vec()).unwrap(),
            [0xd6, 0xd0, 0xce, 0xc4]
        );
        assert!(codec.encode("a😊".as_bytes().to_vec()).is_err());
        assert_eq!(
            codec.encode(vec![0x1b, b'M', 0xff]).unwrap(),
            [0x1b, b'M', 0xff]
        );
    }

    #[test]
    fn utf8_path_keeps_raw_bytes_for_the_vt_parser() {
        let mut codec = TerminalCodec::new(TerminalEncoding::Utf8);
        assert_eq!(codec.decode(&[0xe4]), [0xe4]);
    }
}
