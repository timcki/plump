// sleep_image: load a BMP from SD and convert to 2bpp grayscale
// for display as the deep-sleep wallpaper.
//
// supports uncompressed BMP files (1-bit, 8-bit palette, 24-bit RGB)
// at exactly 480×800. uses Atkinson dithering to quantize to 4 gray
// levels for the SSD1677's dual-plane grayscale mode.
//
// the image is stored in 6 chunks of ~16KB each to avoid needing a
// single 96KB contiguous allocation (the ESP32-C3 has two disjoint
// heap pools of 108KB and 62KB — neither can hold 96KB).

use alloc::vec::Vec;
use log::{debug, info, warn};

use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::sdcard::SdStorage;

const IMG_W: usize = SCREEN_W as usize; // 480
const IMG_H: usize = SCREEN_H as usize; // 800

/// Bytes per row in the 2bpp packed output.
const OUT_STRIDE: usize = IMG_W.div_ceil(4); // 120

/// Number of chunks to split the image into.
/// Each chunk is ~16KB, fitting comfortably in either heap pool.
pub const CHUNK_COUNT: usize = 6;

/// Rows per chunk (last chunk may have fewer).
const ROWS_PER_CHUNK: usize = IMG_H.div_ceil(CHUNK_COUNT); // 134

/// Target cap for a temporary batched SD read buffer.
/// This stays on the heap so we don't eat into the ~11KB stack margin.
const MAX_BATCH_BYTES: usize = 16 * 1024;

/// 2bpp packed grayscale image ready for `blit_2bpp`, stored in chunks.
/// Pixel values: 0=white, 1=light gray, 2=dark gray, 3=black.
pub struct SleepImage {
    pub chunks: [Vec<u8>; CHUNK_COUNT],
    pub width: u16,
    pub height: u16,
    pub stride: u16,
}

/// Per-chunk row count for the 2bpp output layout.
/// Shared by the chunk allocator and `SleepImage::chunk_rows`.
#[inline]
const fn chunk_rows_for(idx: usize) -> usize {
    if idx < CHUNK_COUNT - 1 {
        ROWS_PER_CHUNK
    } else {
        IMG_H - ROWS_PER_CHUNK * (CHUNK_COUNT - 1)
    }
}

/// Allocate a zeroed heap buffer without panicking on OOM;
/// caller logs and decides how to fall back.
#[inline]
fn try_alloc_zeroed(len: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).ok()?;
    buf.resize(len, 0);
    Some(buf)
}

impl SleepImage {
    /// Number of rows in the given chunk.
    #[inline]
    pub fn chunk_rows(&self, chunk: usize) -> usize {
        chunk_rows_for(chunk)
    }

    /// First logical row of the given chunk.
    #[inline]
    pub fn chunk_start_row(&self, chunk: usize) -> usize {
        chunk * ROWS_PER_CHUNK
    }
}

/// File name to look for on the SD card root.
const FILENAME: &str = "SLEEP.BMP";

/// BMP header size (file header 14 + DIB header 40).
const BMP_HEADER_SIZE: usize = 54;

/// Max palette entries we support (8-bit indexed).
const MAX_PALETTE: usize = 256;

/// Convert a BGRA palette slice (`count` × 4 bytes) into a luminance LUT.
/// Uses Rec.601 weights scaled by 256: 77*R + 150*G + 29*B.
fn fill_palette_lum(bgra: &[u8], count: usize, dst: &mut [u8; MAX_PALETTE]) {
    for i in 0..count {
        let off = i * 4;
        let b = bgra[off] as u32;
        let g = bgra[off + 1] as u32;
        let r = bgra[off + 2] as u32;
        dst[i] = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
    }
}

/// Read and parse the BMP header. Returns (pixel_data_offset, bits_per_pixel, palette_lum).
/// `palette_lum` maps each palette index → luminance 0..255 (empty for non-paletted).
fn parse_header(sd: &SdStorage) -> Option<(u32, u16, [u8; MAX_PALETTE])> {
    // read header + full palette in one chunk (54 + 256*4 = 1078 bytes)
    let mut buf = [0u8; BMP_HEADER_SIZE + MAX_PALETTE * 4];
    let n = sd.read_file_chunk(FILENAME, 0, &mut buf).ok()?;
    if n < BMP_HEADER_SIZE {
        info!("sleep_image: file too small for BMP header");
        return None;
    }

    // validate magic
    if buf[0] != 0x42 || buf[1] != 0x4D {
        info!("sleep_image: not a BMP file");
        return None;
    }

    let pixel_offset = u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]);
    let width = i32::from_le_bytes([buf[18], buf[19], buf[20], buf[21]]);
    let height = i32::from_le_bytes([buf[22], buf[23], buf[24], buf[25]]);
    let bpp = u16::from_le_bytes([buf[28], buf[29]]);
    let compression = u32::from_le_bytes([buf[30], buf[31], buf[32], buf[33]]);

    if width != IMG_W as i32 || height != IMG_H as i32 {
        info!(
            "sleep_image: wrong dimensions {}x{} (need {}x{})",
            width, height, IMG_W, IMG_H
        );
        return None;
    }

    if compression != 0 {
        info!(
            "sleep_image: compressed BMP not supported (type={})",
            compression
        );
        return None;
    }

    // palette entry counts per supported bit depth
    let palette_entries = match bpp {
        1 => 2,
        8 => MAX_PALETTE,
        24 => 0,
        _ => {
            info!("sleep_image: unsupported bit depth {}", bpp);
            return None;
        }
    };

    let mut palette_lum = [0u8; MAX_PALETTE];
    if palette_entries > 0 {
        let needed = BMP_HEADER_SIZE + palette_entries * 4;
        if n < needed {
            info!("sleep_image: truncated palette ({}bpp)", bpp);
            return None;
        }
        fill_palette_lum(&buf[BMP_HEADER_SIZE..needed], palette_entries, &mut palette_lum);
    }

    Some((pixel_offset, bpp, palette_lum))
}

#[inline]
fn bmp_row_stride(bpp: u16) -> Option<usize> {
    // BMP rows are padded to a 4-byte boundary
    match bpp {
        8 => Some(IMG_W.next_multiple_of(4)),
        24 => Some((IMG_W * 3).next_multiple_of(4)),
        1 => Some(IMG_W.div_ceil(8).next_multiple_of(4)),
        _ => None,
    }
}

fn alloc_read_batch(row_stride: usize) -> Option<Vec<u8>> {
    let target_rows = (MAX_BATCH_BYTES / row_stride).max(1);
    // halving retry sequence: target, target/2, target/4, ... down to 1 (inclusive)
    let rows_seq = core::iter::successors(
        Some(target_rows),
        |&r| (r > 1).then(|| (r / 2).max(1)),
    );

    let picked = rows_seq
        .inspect(|&rows| {
            if rows < target_rows {
                debug!(
                    "sleep_image: read batch retry at {} rows ({} bytes)",
                    rows,
                    rows * row_stride
                );
            }
        })
        .find_map(|rows| try_alloc_zeroed(rows * row_stride));

    if picked.is_none() {
        info!(
            "sleep_image: failed to allocate even a 1-row read batch ({} bytes)",
            row_stride
        );
    }
    picked
}

/// Quantize a luminance value to the nearest of the 4 gray levels.
/// Returns (quantized_level 0..3, quantized_luminance 0/85/170/255).
#[inline]
fn quantize4(lum: i16) -> (u8, i16) {
    if lum < 43 {
        (3, 0) // black
    } else if lum < 128 {
        (2, 85) // dark gray
    } else if lum < 213 {
        (1, 170) // light gray
    } else {
        (0, 255) // white
    }
}

/// Convert one BMP row already loaded in memory into per-pixel luminance values.
fn decode_row_lum(
    row: &[u8],
    bpp: u16,
    palette_lum: &[u8; MAX_PALETTE],
    lum_out: &mut [i16; IMG_W],
) -> bool {
    match bpp {
        8 => {
            if row.len() < IMG_W {
                return false;
            }
            for x in 0..IMG_W {
                lum_out[x] = palette_lum[row[x] as usize] as i16;
            }
            true
        }
        24 => {
            if row.len() < IMG_W * 3 {
                return false;
            }
            for x in 0..IMG_W {
                let off = x * 3;
                let b = row[off] as u32;
                let g = row[off + 1] as u32;
                let r = row[off + 2] as u32;
                lum_out[x] = ((77 * r + 150 * g + 29 * b) >> 8) as i16;
            }
            true
        }
        1 => {
            if row.len() < IMG_W.div_ceil(8) {
                return false;
            }
            for x in 0..IMG_W {
                let bit = (row[x / 8] >> (7 - (x & 7))) & 1;
                lum_out[x] = palette_lum[bit as usize] as i16;
            }
            true
        }
        _ => false,
    }
}

/// Load `SLEEP.BMP` from SD root and convert to 2bpp grayscale via
/// Atkinson dithering. Returns `None` if the file is missing or invalid.
///
/// Reads BMP rows in heap-backed batches to avoid the 800 open/seek/read
/// cycles of row-at-a-time loading while still keeping stack usage flat.
/// Output is 96,000 bytes (480×800 at 2 bits per pixel) split across
/// 6 chunks of ~16KB each, packed MSB-first (4 pixels per byte).
pub fn load_sleep_image(sd: &SdStorage) -> Option<SleepImage> {
    info!("sleep_image: load_sleep_image start");

    let (pixel_offset, bpp, palette_lum) = parse_header(sd)?;
    let row_stride = bmp_row_stride(bpp)?;
    info!(
        "sleep_image: {}x{} {}bpp, pixel data at offset {}",
        IMG_W, IMG_H, bpp, pixel_offset
    );

    // allocate 6 output chunks (~16KB each, fits in either heap pool).
    // short-circuits on first alloc failure; previously-filled chunks are
    // dropped via the array's Drop when we return None.
    let mut chunks: [Vec<u8>; CHUNK_COUNT] = core::array::from_fn(|_| Vec::new());
    for (i, slot) in chunks.iter_mut().enumerate() {
        let Some(buf) = try_alloc_zeroed(chunk_rows_for(i) * OUT_STRIDE) else {
            warn!("sleep_image: chunk alloc failed, falling back to text sleep screen");
            return None;
        };
        *slot = buf;
    }

    // allocate the temporary read buffer after the output chunks so the
    // permanent image storage gets first pick of the heap; if this extra
    // batch buffer cannot fit we back off to smaller batches instead of OOMing.
    let mut read_batch = alloc_read_batch(row_stride)?;
    let batch_rows = read_batch.len() / row_stride;
    info!(
        "sleep_image: row_stride={} batch_rows={} batch_bytes={}",
        row_stride,
        batch_rows,
        read_batch.len()
    );

    // Atkinson dithering needs error buffers for current + next + next-next row.
    // We process BMP rows bottom-up (row 799..0 in BMP order) and write
    // output top-down (output row 0 = BMP row 799).
    let mut err: [[i16; IMG_W]; 3] = [[0i16; IMG_W]; 3];
    // reused across all rows; decode_row_lum overwrites every position
    let mut lum = [0i16; IMG_W];
    let mut cur = 0usize;
    let mut out_y = 0usize;

    while out_y < IMG_H {
        let rows = batch_rows.min(IMG_H - out_y);
        let first_bmp_row = IMG_H - out_y - rows;
        let batch_len = rows * row_stride;
        let offset = pixel_offset + (first_bmp_row * row_stride) as u32;

        info!(
            "sleep_image: reading batch at row {} (rows={}, offset={})",
            first_bmp_row, rows, offset
        );

        match sd.read_file_chunk(FILENAME, offset, &mut read_batch[..batch_len]) {
            Ok(n) if n == batch_len => {}
            Ok(n) => {
                info!(
                    "sleep_image: short read at row {} (got {}, need {})",
                    first_bmp_row, n, batch_len
                );
                return None;
            }
            Err(_) => {
                info!("sleep_image: read error at row {}", first_bmp_row);
                return None;
            }
        }

        // read a contiguous bottom-up BMP span, then process the rows in
        // reverse so output remains top-down for the dither state machine.
        for batch_row in (0..rows).rev() {
            let bmp_row = first_bmp_row + batch_row;
            let row = &read_batch[batch_row * row_stride..(batch_row + 1) * row_stride];

            if !decode_row_lum(row, bpp, &palette_lum, &mut lum) {
                info!("sleep_image: decode error at row {}", bmp_row);
                return None;
            }

            // add accumulated error from previous rows
            for x in 0..IMG_W {
                lum[x] = (lum[x] + err[cur][x]).clamp(0, 255);
            }

            // clear current error row for reuse as the row-after-next
            err[cur].fill(0);

            let next = (cur + 1) % 3;
            let next2 = (cur + 2) % 3;

            // find the correct chunk and row within it
            let chunk_idx = (out_y / ROWS_PER_CHUNK).min(CHUNK_COUNT - 1);
            let row_in_chunk = out_y - chunk_idx * ROWS_PER_CHUNK;
            let out_row =
                &mut chunks[chunk_idx][row_in_chunk * OUT_STRIDE..(row_in_chunk + 1) * OUT_STRIDE];

            // quantize + Atkinson error diffusion
            for x in 0..IMG_W {
                let (level, quant_lum) = quantize4(lum[x]);
                let error = (lum[x] - quant_lum) / 8;

                // pack 2bpp: MSB first, 4 pixels per byte
                let byte_idx = x / 4;
                let shift = 6 - (x & 3) * 2;
                out_row[byte_idx] |= level << shift;

                // Atkinson diffusion to 6 neighbors:
                //       *   1/8  1/8
                // 1/8  1/8  1/8
                //       1/8
                if error != 0 {
                    if x + 1 < IMG_W {
                        lum[x + 1] += error;
                    }
                    if x + 2 < IMG_W {
                        lum[x + 2] += error;
                    }
                    if x > 0 {
                        err[next][x - 1] += error;
                    }
                    err[next][x] += error;
                    if x + 1 < IMG_W {
                        err[next][x + 1] += error;
                    }
                    err[next2][x] += error;
                }
            }

            cur = next;
            out_y += 1;
        }
    }

    info!(
        "sleep_image: converted to 2bpp ({} chunks, {} bytes total)",
        CHUNK_COUNT,
        chunks.iter().map(|c| c.len()).sum::<usize>()
    );
    Some(SleepImage {
        chunks,
        width: IMG_W as u16,
        height: IMG_H as u16,
        stride: OUT_STRIDE as u16,
    })
}
