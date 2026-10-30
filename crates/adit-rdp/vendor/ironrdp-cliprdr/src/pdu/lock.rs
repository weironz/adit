use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, cast_int, ensure_fixed_part_size,
    invalid_field_err,
};
use ironrdp_pdu::impl_pdu_pod;

use crate::pdu::PartialHeader;

/// Represents `CLIPRDR_LOCK_CLIPDATA`/`CLIPRDR_UNLOCK_CLIPDATA`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockDataId(pub u32);

impl_pdu_pod!(LockDataId);

impl LockDataId {
    const NAME: &'static str = "CLIPRDR_(UN)LOCK_CLIPDATA";
    const FIXED_PART_SIZE: usize = 4 /* Id */;
}

impl Encode for LockDataId {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let header = PartialHeader::new(cast_int!("dataLen", Self::FIXED_PART_SIZE)?);
        header.encode(dst)?;

        ensure_fixed_part_size!(in: dst);
        dst.write_u32(self.0);

        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        PartialHeader::SIZE + Self::FIXED_PART_SIZE
    }
}

impl<'de> Decode<'de> for LockDataId {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let header = PartialHeader::decode(src)?;

        // MS-RDPECLIP Appendix A: some Windows versions append four bytes
        // outside dataLen. Never read those bytes as the lock ID.
        let trailing = src.len().checked_sub(header.data_length());
        if header.data_length() != Self::FIXED_PART_SIZE
            || !matches!(trailing, Some(0 | 4))
            || !header.message_flags.is_empty()
        {
            return Err(invalid_field_err!(
                "header",
                "invalid lock request header or body length"
            ));
        }

        ensure_fixed_part_size!(in: src);
        let id = src.read_u32();
        if trailing == Some(4) {
            let _ = src.read_slice(4);
        }

        Ok(Self(id))
    }
}
