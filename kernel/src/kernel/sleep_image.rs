// sleep_image: load a BMP from SD and convert to 2bpp grayscale
// for display as the deep-sleep wallpaper.
//
// supports uncompressed BMP files (1-bit, 8-bit palette, 24-bit RGB)
// at exactly 480×800. uses Atkinson dithering to quantize to 4 gray
// levels for the SSD1677's dual-plane grayscale mode.

use alloc::vec;
use alloc::vec::Vec;
use log::info;

use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage;

const IMG_W: usize = SCREEN_W as usize; // 480
const IMG_H: usize = SCREEN_H as usize; // 800

/// 2bpp packed grayscale image ready for `blit_2bpp`.
/// Pixel values: 0=white, 1=light gray, 2=dark gray, 3=black.
pub struct SleepImage {
    pub data: Vec<u8>,
    pub width: u16,
    pub height: u16,
    /// Bytes per row in the packed 2bpp output (width / 4).
    pub stride: u16,
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
        info!("sleep_image: compressed BMP not supported (type={})", compression);
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
        // 1-bit: 2-entry palette
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

/// Quantize a luminance value to the nearest of the 4 gray levels.
/// Returns (quantized_level 0..3, quantized_luminance 0/85/170/255).
#[inline]
fn quantize4(lum: i16) -> (u8, i16) {
    // thresholds: midpoints between levels 0, 85, 170, 255
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

/// Read BMP row data for a single row and return luminance values.
/// BMP rows are bottom-up, so `bmp_row` 0 is the bottom of the image.
/// Returns the number of pixels written (should be IMG_W).
fn read_row_lum(
    sd: &SdStorage,
    pixel_offset: u32,
    bmp_row: usize,
    bpp: u16,
    palette_lum: &[u8; MAX_PALETTE],
    lum_out: &mut [i16; IMG_W],
) -> bool {
    match bpp {
        8 => {
            let row_stride = IMG_W; // 480 bytes, already 4-byte aligned
            let offset = pixel_offset + (bmp_row * row_stride) as u32;
            let mut row_buf = [0u8; IMG_W];
            if storage::read_file_chunk(sd, FILENAME, offset, &mut row_buf).is_err() {
                return false;
            }
            for x in 0..IMG_W {
                lum_out[x] = palette_lum[row_buf[x] as usize] as i16;
            }
            true
        }
        24 => {
            let row_stride = (IMG_W * 3 + 3) & !3; // pad to 4 bytes
            let offset = pixel_offset + (bmp_row * row_stride) as u32;
            let mut row_buf = [0u8; IMG_W * 3 + 3]; // 1443 bytes max
            let read_len = row_stride;
            if storage::read_file_chunk(sd, FILENAME, offset, &mut row_buf[..read_len]).is_err() {
                return false;
            }
            for x in 0..IMG_W {
                let off = x * 3;
                let b = row_buf[off] as u32;
                let g = row_buf[off + 1] as u32;
                let r = row_buf[off + 2] as u32;
                lum_out[x] = ((77 * r + 150 * g + 29 * b) >> 8) as i16;
            }
            true
        }
        1 => {
            let row_stride = ((IMG_W + 31) / 32) * 4; // pad to 4 bytes
            let offset = pixel_offset + (bmp_row * row_stride) as u32;
            let mut row_buf = [0u8; (IMG_W + 7) / 8 + 4]; // 64 bytes
            if storage::read_file_chunk(sd, FILENAME, offset, &mut row_buf[..row_stride]).is_err()
            {
                return false;
            }
            for x in 0..IMG_W {
                let bit = (row_buf[x / 8] >> (7 - (x & 7))) & 1;
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
/// Reads row-by-row from SD to avoid holding the full BMP in RAM.
/// Output is 96,000 bytes (480×800 at 2 bits per pixel), top-down,
/// packed MSB-first (4 pixels per byte).
pub fn load_sleep_image(sd: &SdStorage) -> Option<SleepImage> {
    let (pixel_offset, bpp, palette_lum) = parse_header(sd)?;
    info!(
        "sleep_image: {}x{} {}bpp, pixel data at offset {}",
        IMG_W, IMG_H, bpp, pixel_offset
    );

    let out_stride = IMG_W / 4; // 120 bytes per row
    let mut data: Vec<u8> = vec![0u8; out_stride * IMG_H]; // 96,000 bytes

    // Atkinson dithering needs error buffers for current + next + next-next row.
    // We process BMP rows bottom-up (row 799..0 in BMP order) and write
    // output top-down (output row 0 = BMP row 799).
    //
    // error buffers are indexed 0/1/2 and rotated each row
    let mut err: [[i16; IMG_W]; 3] = [[0i16; IMG_W]; 3];
    let mut cur = 0usize; // index of current error row

    for out_y in 0..IMG_H {
        // BMP row for this output row (bottom-up → top-down flip)
        let bmp_row = IMG_H - 1 - out_y;

        // read raw luminance
        let mut lum = [0i16; IMG_W];
        if !read_row_lum(sd, pixel_offset, bmp_row, bpp, &palette_lum, &mut lum) {
            info!("sleep_image: read error at row {}", bmp_row);
            return None;
        }

        // add accumulated error from previous rows
        for x in 0..IMG_W {
            lum[x] = (lum[x] + err[cur][x]).clamp(0, 255);
        }

        // clear current error row for reuse as the row-after-next
        err[cur] = [0i16; IMG_W];

        let next = (cur + 1) % 3;
        let next2 = (cur + 2) % 3;

        // quantize + Atkinson error diffusion
        let out_row = &mut data[out_y * out_stride..(out_y + 1) * out_stride];
        for x in 0..IMG_W {
            let (level, quant_lum) = quantize4(lum[x]);
            let error = (lum[x] - quant_lum) / 8;

            // pack 2bpp: MSB first, 4 pixels per byte
            // pixel 0 in bits 7:6, pixel 1 in bits 5:4, etc.
            let byte_idx = x / 4;
            let shift = 6 - (x & 3) * 2;
            out_row[byte_idx] |= level << shift;

            // Atkinson diffusion to 6 neighbors:
            //       *   1/8  1/8
            // 1/8  1/8  1/8
            //       1/8
            if error != 0 {
                if x + 1 < IMG_W {
                    lum[x + 1] += error; // (x+1, y) — still in current row
                }
                if x + 2 < IMG_W {
                    lum[x + 2] += error; // (x+2, y) — still in current row
                }
                if x > 0 {
                    err[next][x - 1] += error; // (x-1, y+1)
                }
                err[next][x] += error; // (x, y+1)
                if x + 1 < IMG_W {
                    err[next][x + 1] += error; // (x+1, y+1)
                }
                err[next2][x] += error; // (x, y+2)
            }
        }

        cur = next;
    }

    info!("sleep_image: converted to 2bpp ({} bytes)", data.len());
    Some(SleepImage {
        data,
        width: IMG_W as u16,
        height: IMG_H as u16,
        stride: out_stride as u16,
    })
}
