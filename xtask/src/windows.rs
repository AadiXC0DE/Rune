//! Reproducible single-executable Windows ZIP releases, without a ZIP dependency.
//! Record layouts: https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT

use std::io::Write;

pub(super) fn archive(binary: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(binary).map_err(|err| err.to_string())?;
    let compressed = encoder.finish().map_err(|err| err.to_string())?;
    let size = u32::try_from(binary.len()).map_err(|_| "ZIP executable is too large")?;
    let packed = u32::try_from(compressed.len()).map_err(|_| "ZIP payload is too large")?;
    let mut crc = flate2::Crc::new();
    crc.update(binary);

    let mut local = [0_u8; 30];
    local[..4].copy_from_slice(b"PK\x03\x04");
    local[4..6].copy_from_slice(&20_u16.to_le_bytes());
    local[8..10].copy_from_slice(&8_u16.to_le_bytes()); // Deflate.
    local[12..14].copy_from_slice(&33_u16.to_le_bytes()); // 1980-01-01, midnight.
    local[14..18].copy_from_slice(&crc.sum().to_le_bytes());
    local[18..22].copy_from_slice(&packed.to_le_bytes());
    local[22..26].copy_from_slice(&size.to_le_bytes());
    local[26..28].copy_from_slice(&8_u16.to_le_bytes()); // rune.exe.
    let mut out = local.to_vec();
    out.extend_from_slice(b"rune.exe");
    out.extend_from_slice(&compressed);
    let offset = u32::try_from(out.len()).map_err(|_| "ZIP offset is too large")?;

    let mut central = [0_u8; 46];
    central[..4].copy_from_slice(b"PK\x01\x02");
    central[4..6].copy_from_slice(&20_u16.to_le_bytes()); // DOS creator, version 2.0.
    central[6..32].copy_from_slice(&local[4..30]);
    // No extras, comments, attributes, disk number, or local header offset.
    out.extend_from_slice(&central);
    out.extend_from_slice(b"rune.exe");

    let mut end = [0_u8; 22];
    end[..4].copy_from_slice(b"PK\x05\x06");
    end[8..10].copy_from_slice(&1_u16.to_le_bytes());
    end[10..12].copy_from_slice(&1_u16.to_le_bytes());
    end[12..16].copy_from_slice(&54_u32.to_le_bytes());
    end[16..20].copy_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&end);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn windows_zip_is_reproducible_and_readable_by_an_independent_reader() {
        let payload = b"executable contents".repeat(64);
        let bytes = archive(&payload).expect("ZIP");
        assert_eq!(bytes, archive(&payload).expect("repeat ZIP"));
        assert!(bytes.len() < payload.len());
        // Python's standard ZIP reader checks structure, Deflate and CRC.
        let python = if cfg!(windows) { "python" } else { "python3" };
        let mut child = Command::new(python)
            .args(["-c", "import io,sys,zipfile; z=zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())); assert z.namelist()==['rune.exe']; i=z.getinfo('rune.exe'); assert i.date_time==(1980,1,1,0,0,0); assert i.compress_type==8; assert i.extra==b''; assert z.read(i)==b'executable contents'*64"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("Python ZIP reader");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(&bytes)
            .expect("ZIP input");
        assert!(child.wait().expect("reader exit").success());
    }

    #[test]
    fn only_windows_targets_select_zip() {
        for target in [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-pc-windows-gnu",
        ] {
            assert_eq!(super::super::archive_extension(target), "zip");
        }
        for target in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"] {
            assert_eq!(super::super::archive_extension(target), "tar.gz");
        }
    }
}
