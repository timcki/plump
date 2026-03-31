// Cover thumbnail cache: shared helpers for reading and writing
// persistent 1-bit cover thumbnails on SD.
//
// Format is identical to the reader's inline image cache:
//   4-byte header: u16 width (LE), u16 height (LE)
//   packed 1-bit pixel payload (stride = ceil(width/8))
//
// Thumbnails live in the per-book `_PULP/_XXXXXXX/` directory
// alongside chapter/image caches, under a fixed filename.

use alloc::vec::Vec;

use smol_epub::cache;

use crate::kernel::KernelHandle;
use crate::kernel::work_queue::DecodedImage;

/// Fixed filename for the cover thumbnail inside the per-book cache directory.
pub const COVER_THUMB_FILE: &str = "COVER.BIN";

/// Maximum width for the cover thumbnail (fits inside the home-screen card).
pub const COVER_THUMB_MAX_W: u16 = 200;

/// Maximum height for the cover thumbnail (fits inside the home-screen card).
pub const COVER_THUMB_MAX_H: u16 = 240;

/// Compute the per-book cache directory name from a book filename.
///
/// Returns the 8-byte directory name buffer (e.g. `_1A2B3C4D`) that
/// can be passed to `cache::dir_name_str()`.
fn cache_dir_for_filename(filename: &[u8]) -> [u8; 8] {
    let hash = cache::fnv1a(filename);
    cache::dir_name_for_hash(hash)
}

/// Save a 1-bit cover thumbnail to the per-book cache directory.
pub fn save_cover_thumb(
    k: &mut KernelHandle<'_>,
    dir: &str,
    img: &DecodedImage,
) -> crate::error::Result<()> {
    let mut header = [0u8; 4];
    header[0..2].copy_from_slice(&img.width.to_le_bytes());
    header[2..4].copy_from_slice(&img.height.to_le_bytes());
    k.sd().write_in_pulp_subdir(dir, COVER_THUMB_FILE, &header)?;
    k.sd().append_in_pulp_subdir(dir, COVER_THUMB_FILE, &img.data)?;
    Ok(())
}

/// Load a 1-bit cover thumbnail from the per-book cache directory.
///
/// Returns `None` if the file doesn't exist or is invalid.
pub fn load_cover_thumb(k: &mut KernelHandle<'_>, dir: &str) -> Option<DecodedImage> {
    let size = k.sd().file_size_in_pulp_subdir(dir, COVER_THUMB_FILE).ok()?;
    if size < 5 {
        return None;
    }
    let mut header = [0u8; 4];
    k.sd().read_chunk_in_pulp_subdir(dir, COVER_THUMB_FILE, 0, &mut header)
        .ok()?;
    let width = u16::from_le_bytes([header[0], header[1]]);
    let height = u16::from_le_bytes([header[2], header[3]]);
    if width == 0 || height == 0 {
        return None;
    }
    let stride = (width as usize).div_ceil(8);
    let data_len = stride * height as usize;
    if size as usize != 4 + data_len {
        return None;
    }
    let mut data = Vec::new();
    data.try_reserve_exact(data_len).ok()?;
    data.resize(data_len, 0);
    k.sd().read_chunk_in_pulp_subdir(dir, COVER_THUMB_FILE, 4, &mut data)
        .ok()?;
    Some(DecodedImage {
        width,
        height,
        data,
        stride,
    })
}

/// Check whether a cover thumbnail already exists for the given cache dir.
pub fn has_cover_thumb(k: &mut KernelHandle<'_>, dir: &str) -> bool {
    k.sd().file_size_in_pulp_subdir(dir, COVER_THUMB_FILE)
        .map(|s| s >= 5)
        .unwrap_or(false)
}

// ── convenience helpers (filename → hash → dir → operation) ─────────

/// Load a cover thumbnail by book filename (internalizes hash→dir).
pub fn load_cover_for(k: &mut KernelHandle<'_>, filename: &[u8]) -> Option<DecodedImage> {
    let dir_buf = cache_dir_for_filename(filename);
    let dir = cache::dir_name_str(&dir_buf);
    load_cover_thumb(k, dir)
}

/// Check whether a cover thumbnail exists by book filename.
pub fn has_cover_for(k: &mut KernelHandle<'_>, filename: &[u8]) -> bool {
    let dir_buf = cache_dir_for_filename(filename);
    let dir = cache::dir_name_str(&dir_buf);
    has_cover_thumb(k, dir)
}
