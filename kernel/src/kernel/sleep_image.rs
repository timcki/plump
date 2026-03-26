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

use alloc::vec;
use alloc::vec::Vec;
use log::{debug, info};

use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage;

const IMG_W: usize = SCREEN_W as usize; // 480
const IMG_H: usize = SCREEN_H as usize; // 800

/// Bytes per row in the 2bpp packed output.
const OUT_STRIDE: usize = IMG_W / 4; // 120

/// Number of chunks to split the image into.
/// Each chunk is ~16KB, fitting comfortably in either heap pool.
pub const CHUNK_COUNT: usize = 6;

/// Rows per chunk (last chunk may have fewer).
const ROWS_PER_CHUNK: usize = (IMG_H + CHUNK_COUNT - 1) / CHUNK_COUNT; // 134

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

impl SleepImage {
    /// Number of rows in the given chunk.
    #[inline]
    pub fn chunk_rows(&self, chunk: usize) -> usize {
        if chunk < CHUNK_COUNT - 1 {
            ROWS_PER_CHUNK
        } else {
            IMG_H - ROWS_PER_CHUNK * (CHUNK_COUNT - 1)
        }
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

/// Read and parse the BMP header. Returns (pixel_data_offset, bits_per_pixel, palette_lum).
/// `palette_lum` maps each palette index → luminance 0..255 (empty for non-paletted).
fn parse_header(sd: &SdStorage) -> Option<(u32, u16, [u8; MAX_PALETTE])> {
    // read header + full palette in one chunk (54 + 256*4 = 1078 bytes)
    let mut buf = [0u8; BMP_HEADER_SIZE + MAX_PALETTE * 4];
    let n = storage::read_file_chunk(sd, FILENAME, 0, &mut buf).ok()?;
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

    if bpp != 1 && bpp != 8 && bpp != 24 {
        info!("sleep_image: unsupported bit depth {}", bpp);
        return None;
    }

    // build palette luminance LUT
    let mut palette_lum = [0u8; MAX_PALETTE];
    if bpp == 8 {
        let palette_start = BMP_HEADER_SIZE;
        if n < palette_start + MAX_PALETTE * 4 {
            info!("sleep_image: truncated palette");
            return None;
        }
        for i in 0..MAX_PALETTE {
            let off = palette_start + i * 4;
            let b = buf[off] as u32;
            let g = buf[off + 1] as u32;
            let r = buf[off + 2] as u32;
            palette_lum[i] = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
        }
    } else if bpp == 1 {
        let palette_start = BMP_HEADER_SIZE;
        if n < palette_start + 2 * 4 {
            info!("sleep_image: truncated 1-bit palette");
            return None;
        }
        for i in 0..2 {
            let off = palette_start + i * 4;
            let b = buf[off] as u32;
            let g = buf[off + 1] as u32;
            let r = buf[off + 2] as u32;
            palette_lum[i] = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
        }
    }

    Some((pixel_offset, bpp, palette_lum))
}

#[inline]
fn bmp_row_stride(bpp: u16) -> Option<usize> {
    match bpp {
        8 => Some(IMG_W),
        24 => Some((IMG_W * 3 + 3) & !3),
        1 => Some(((IMG_W + 31) / 32) * 4),
        _ => None,
    }
}

fn alloc_read_batch(row_stride: usize) -> Option<Vec<u8>> {
    let mut rows = (MAX_BATCH_BYTES / row_stride).max(1);
    let target_rows = rows;

    loop {
        let len = rows * row_stride;
        let mut buf = Vec::new();
        match buf.try_reserve_exact(len) {
            Ok(()) => {
                buf.resize(len, 0);
                if rows < target_rows {
                    debug!(
                        "sleep_image: using reduced read batch ({} rows, {} bytes)",
                        rows, len
                    );
                }
                return Some(buf);
            }
            Err(_) if rows > 1 => {
                let next_rows = (rows / 2).max(1);
                debug!(
                    "sleep_image: read batch alloc failed at {} bytes, retrying with {} rows",
                    len, next_rows
                );
                rows = next_rows;
            }
            Err(_) => {
                info!(
                    "sleep_image: failed to allocate even a 1-row read batch ({} bytes)",
                    len
                );
                return None;
            }
        }
    }
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
            let min_len = (IMG_W + 7) / 8;
            if row.len() < min_len {
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
    let (pixel_offset, bpp, palette_lum) = parse_header(sd)?;
    let row_stride = bmp_row_stride(bpp)?;
    debug!(
        "sleep_image: {}x{} {}bpp, pixel data at offset {}",
        IMG_W, IMG_H, bpp, pixel_offset
    );

    // allocate 6 chunks — each ~16KB, fits in either heap pool
    let last_chunk_rows = IMG_H - ROWS_PER_CHUNK * (CHUNK_COUNT - 1);
    let mut chunks: [Vec<u8>; CHUNK_COUNT] = core::array::from_fn(|i| {
        let rows = if i < CHUNK_COUNT - 1 {
            ROWS_PER_CHUNK
        } else {
            last_chunk_rows
        };
        vec![0u8; rows * OUT_STRIDE]
    });

    // allocate the temporary read buffer after the output chunks so the
    // permanent image storage gets first pick of the heap; if this extra
    // batch buffer cannot fit we back off to smaller batches instead of OOMing.
    let mut read_batch = alloc_read_batch(row_stride)?;
    let batch_rows = read_batch.len() / row_stride;
    debug!(
        "sleep_image: row_stride={} batch_rows={} batch_bytes={}",
        row_stride,
        batch_rows,
        read_batch.len()
    );

    // Atkinson dithering needs error buffers for current + next + next-next row.
    // We process BMP rows bottom-up (row 799..0 in BMP order) and write
    // output top-down (output row 0 = BMP row 799).
    let mut err: [[i16; IMG_W]; 3] = [[0i16; IMG_W]; 3];
    let mut cur = 0usize;
    let mut out_y = 0usize;

    while out_y < IMG_H {
        let rows = batch_rows.min(IMG_H - out_y);
        let first_bmp_row = IMG_H - out_y - rows;
        let batch_len = rows * row_stride;
        let offset = pixel_offset + (first_bmp_row * row_stride) as u32;

        match storage::read_file_chunk(sd, FILENAME, offset, &mut read_batch[..batch_len]) {
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

            let mut lum = [0i16; IMG_W];
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

    debug!(
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
