//! Reads the single-member Deflate ZIP format emitted by Windows release staging.
//! No archive member path is used as a filesystem destination.

use super::{MAX_ARTIFACT_BYTES, invalid_archive};
use rune_core::error::{Result, RuneError};

pub(super) fn release_binary(bytes: &[u8]) -> Result<Vec<u8>> {
    // Reject ZIP64, split archives, extras, comments and data descriptors. The
    // release writer never emits them. Validate both copies of member metadata.
    let local = bytes
        .get(..30)
        .ok_or_else(|| invalid_archive("truncated ZIP header"))?;
    if &local[..4] != b"PK\x03\x04"
        || u16_at(local, 4) != 20
        || u16_at(local, 6) != 0
        || u16_at(local, 8) != 8
        || u16_at(local, 26) != 8
        || u16_at(local, 28) != 0
        || bytes.get(30..38) != Some(b"rune.exe".as_slice())
    {
        return Err(invalid_archive(
            "expected a single Deflate `rune.exe` member at the ZIP root",
        ));
    }
    let size = u32_at(local, 22);
    if size == 0 {
        return Err(invalid_archive("the executable member is empty"));
    }
    if u64::from(size) > MAX_ARTIFACT_BYTES {
        return Err(RuneError::too_large(
            "archive executable",
            size as usize,
            MAX_ARTIFACT_BYTES as usize,
        ));
    }
    let packed = u32_at(local, 18) as usize;
    let central_offset = 38_usize
        .checked_add(packed)
        .ok_or_else(|| invalid_archive("ZIP offset overflow"))?;
    let central_end = central_offset
        .checked_add(54)
        .ok_or_else(|| invalid_archive("ZIP offset overflow"))?;
    let central = bytes
        .get(central_offset..central_end)
        .ok_or_else(|| invalid_archive("truncated ZIP directory"))?;
    let end = bytes
        .get(central_end..)
        .ok_or_else(|| invalid_archive("missing ZIP end record"))?;
    // DOS attributes may not mark a directory. Unix attributes may only mark a
    // regular file or leave the file type unspecified.
    let attributes = u32_at(central, 38);
    let file_type = (attributes >> 16) & 0o170_000;
    if &central[..4] != b"PK\x01\x02"
        || central[6..32] != local[4..30]
        || central[32..38].iter().any(|byte| *byte != 0)
        || attributes & 0x10 != 0
        || !matches!(file_type, 0 | 0o100_000)
        || u32_at(central, 42) != 0
        || &central[46..] != b"rune.exe"
        || end.len() != 22
        || &end[..4] != b"PK\x05\x06"
        || end[4..8] != [0; 4]
        || u16_at(end, 8) != 1
        || u16_at(end, 10) != 1
        || u32_at(end, 12) != 54
        || u32_at(end, 16) as usize != central_offset
        || u16_at(end, 20) != 0
    {
        return Err(invalid_archive(
            "ZIP must contain exactly one regular executable with matching headers",
        ));
    }
    let compressed = &bytes[38..central_offset];
    let mut decoder = flate2::Decompress::new(false);
    let mut binary = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(
                &compressed[before_in as usize..],
                &mut buffer,
                flate2::FlushDecompress::None,
            )
            .map_err(|err| invalid_archive(format!("ZIP Deflate stream is corrupt: {err}")))?;
        if decoder.total_out() > u64::from(size) {
            return Err(invalid_archive("ZIP executable exceeds its declared size"));
        }
        let produced = decoder.total_out().saturating_sub(before_out) as usize;
        binary.extend_from_slice(&buffer[..produced]);
        if status == flate2::Status::StreamEnd {
            break;
        }
        if decoder.total_in() == before_in && decoder.total_out() == before_out {
            return Err(invalid_archive("ZIP Deflate stream is truncated"));
        }
    }
    let mut crc = flate2::Crc::new();
    crc.update(&binary);
    if binary.len() != size as usize
        || decoder.total_in() != packed as u64
        || crc.sum() != u32_at(local, 14)
    {
        return Err(invalid_archive(
            "ZIP executable size, compressed stream or CRC does not match",
        ));
    }
    Ok(binary)
}

// Callers pass fixed-size, validated record slices.
fn u16_at(record: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([record[offset], record[offset.saturating_add(1)]])
}

fn u32_at(record: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        record[offset],
        record[offset.saturating_add(1)],
        record[offset.saturating_add(2)],
        record[offset.saturating_add(3)],
    ])
}
