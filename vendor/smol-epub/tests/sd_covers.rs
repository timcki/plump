//! Decode each cover JPEG actually present on the SD card to identify
//! which (if any) trip the decoder. Mirrors the device call path:
//! ChunkReader-streamed reads with the cover-thumb size cap.

use smol_epub::jpeg::{ChunkReader, decode_jpeg};

const COVER_W: u16 = 240;
const COVER_H: u16 = 360;

fn decode(name: &str, bytes: &[u8]) -> Result<(u16, u16), String> {
    let read_fn = |off: u32, buf: &mut [u8]| -> Result<usize, &'static str> {
        let off = off as usize;
        if off >= bytes.len() {
            return Ok(0);
        }
        let n = buf.len().min(bytes.len() - off);
        buf[..n].copy_from_slice(&bytes[off..off + n]);
        Ok(n)
    };
    let reader = ChunkReader::new(read_fn, 0, bytes.len() as u32);
    let _ = env_logger::builder().is_test(true).try_init();
    decode_jpeg(reader, COVER_W, COVER_H)
        .map(|i| (i.width, i.height))
        .map_err(|e| format!("{name}: {e}"))
}

#[test]
fn sd_covers_decode() {
    let cases: &[(&str, &[u8])] = &[
        ("leviathan", include_bytes!("fixtures_sd/leviathan.jpg")),
        ("moonbound", include_bytes!("fixtures_sd/moonbound.jpg")),
        ("artemis", include_bytes!("fixtures_sd/artemis.jpg")),
        ("deathsen", include_bytes!("fixtures_sd/deathsen.jpg")),
        ("homehabi", include_bytes!("fixtures_sd/homehabi.jpg")),
        ("piotrzyc", include_bytes!("fixtures_sd/piotrzyc.jpg")),
        ("storieso", include_bytes!("fixtures_sd/storieso.jpg")),
    ];
    let mut failures = Vec::new();
    for (name, bytes) in cases {
        match decode(name, bytes) {
            Ok((w, h)) => eprintln!("{name}: ok {w}x{h}"),
            Err(e) => {
                eprintln!("{name}: FAIL: {e}");
                failures.push(format!("{name}: {e}"));
            }
        }
    }
    if !failures.is_empty() {
        panic!("decode failures:\n{}", failures.join("\n"));
    }
}
