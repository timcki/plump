// cover thumbnail helpers: read and write pre-dithered 1-bit covers
// inside the per-book bundle.
//
// v1 stores a single `Card` variant (matching the previous behavior);
// Phase 5 extends the covers section with additional sizes (tiny,
// small, detail) and the raw source bytes.
//
// the covers section layout inside a bundle (appended to bundle tail
// the first time a cover is generated, then overwritten in place on
// later regenerations):
//
//   [CoversHeader      12 bytes]
//   [CoverVariant[0]   16 bytes per entry]  variant_count entries
//   [variant[0] bitmap data]                1-bit packed, stride = w/8
//   ...

use alloc::vec::Vec;

use smol_epub::cache;

use plump_kernel::kernel::bundle;

use crate::kernel::KernelHandle;
use crate::kernel::work_queue::DecodedImage;

/// Maximum width for the Card cover variant (fits the home-screen card).
pub const COVER_THUMB_MAX_W: u16 = 200;

/// Maximum height for the Card cover variant.
pub const COVER_THUMB_MAX_H: u16 = 240;

/// Write the Card variant of a cover for the given book (by filename).
///
/// The bundle must exist (chapter caching initializes it). Appends or
/// overwrites the covers section at the bundle tail and updates the
/// bundle header with covers_offset/covers_size and COVERS_READY.
pub fn save_cover_thumb_for(
    k: &mut KernelHandle<'_>,
    filename: &[u8],
    img: &DecodedImage,
) -> crate::error::Result<()> {
    let name_hash = cache::fnv1a(filename);
    save_cover_thumb(k, name_hash, img)
}

/// Write the Card variant of a cover into the bundle identified by
/// `name_hash`.
pub fn save_cover_thumb(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    img: &DecodedImage,
) -> crate::error::Result<()> {
    let mut hdr = bundle::read_header(k.sd(), name_hash)
        .ok_or_else(|| crate::error::Error::new(
            crate::error::ErrorKind::NotFound,
            "save_cover_thumb: bundle missing",
        ))?;

    // section layout: CoversHeader + 1 variant entry + bitmap data
    let hdr_bytes = bundle::COVERS_HDR_SIZE as u32;
    let var_bytes = bundle::COVER_VARIANT_SIZE as u32;
    let data_bytes = img.data.len() as u32;
    let section_size = hdr_bytes + var_bytes + data_bytes;

    // place covers at EOF (first time) or at the existing covers_offset
    // (on regeneration). overwriting in place keeps the file compact when
    // the new section is <= the old one; when it's larger, trailing bytes
    // past (covers_offset + section_size) become dead space but are
    // never read because the header's covers_size is authoritative.
    let covers_offset = if hdr.covers_offset != 0 {
        hdr.covers_offset
    } else {
        bundle::file_size(k.sd(), name_hash).unwrap_or(bundle::HEADER_SIZE as u32)
    };

    // write CoversHeader
    let covers_hdr = bundle::CoversHeader {
        variant_count: 1,
        raw_format: bundle::RAW_FMT_NONE,
        raw_offset: 0,
        raw_size: 0,
    };
    bundle::write_at(k.sd(), name_hash, covers_offset, &covers_hdr.encode())?;

    // write variant entry (offsets within the section)
    let data_offset_in_section = hdr_bytes + var_bytes;
    let variant = bundle::CoverVariant {
        kind: bundle::COVER_KIND_CARD,
        width: img.width,
        height: img.height,
        stride: img.stride as u16,
        data_offset: data_offset_in_section,
        data_size: data_bytes,
    };
    bundle::write_at(
        k.sd(),
        name_hash,
        covers_offset + hdr_bytes,
        &variant.encode(),
    )?;

    // write bitmap bytes
    bundle::write_at(
        k.sd(),
        name_hash,
        covers_offset + data_offset_in_section,
        &img.data,
    )?;

    // update bundle header
    hdr.covers_offset = covers_offset;
    hdr.covers_size = section_size;
    hdr.set_flag(bundle::FLAG_COVERS_READY, true);
    bundle::write_header(k.sd(), name_hash, &hdr)?;

    Ok(())
}

/// Load the Card variant (only variant in v1) from the bundle.
pub fn load_cover_thumb(k: &mut KernelHandle<'_>, name_hash: u32) -> Option<DecodedImage> {
    let hdr = bundle::read_header(k.sd(), name_hash)?;
    if !hdr.has_flag(bundle::FLAG_COVERS_READY)
        || hdr.covers_offset == 0
        || hdr.covers_size < (bundle::COVERS_HDR_SIZE + bundle::COVER_VARIANT_SIZE) as u32
    {
        return None;
    }

    let mut covers_hdr_buf = [0u8; bundle::COVERS_HDR_SIZE];
    bundle::read_at(k.sd(), name_hash, hdr.covers_offset, &mut covers_hdr_buf).ok()?;
    let covers_hdr = bundle::CoversHeader::decode(&covers_hdr_buf)?;
    if covers_hdr.variant_count == 0 {
        return None;
    }

    // pick the first variant whose kind is Card; fall back to variant 0
    // (v1 writes exactly one Card variant, so variant 0 is always it)
    let mut var_buf = [0u8; bundle::COVER_VARIANT_SIZE];
    let mut chosen: Option<bundle::CoverVariant> = None;
    for i in 0..covers_hdr.variant_count as u32 {
        let off = hdr.covers_offset + bundle::COVERS_HDR_SIZE as u32
            + i * bundle::COVER_VARIANT_SIZE as u32;
        bundle::read_at(k.sd(), name_hash, off, &mut var_buf).ok()?;
        let v = bundle::CoverVariant::decode(&var_buf)?;
        if chosen.is_none() || v.kind == bundle::COVER_KIND_CARD {
            chosen = Some(v);
            if v.kind == bundle::COVER_KIND_CARD {
                break;
            }
        }
    }
    let variant = chosen?;
    if variant.width == 0 || variant.height == 0 {
        return None;
    }

    let data_len = variant.data_size as usize;
    let mut data = Vec::new();
    data.try_reserve_exact(data_len).ok()?;
    data.resize(data_len, 0);
    bundle::read_at(
        k.sd(),
        name_hash,
        hdr.covers_offset + variant.data_offset,
        &mut data,
    )
    .ok()?;

    Some(DecodedImage {
        width: variant.width,
        height: variant.height,
        data,
        stride: variant.stride as usize,
    })
}

/// Whether a cover has been generated for this book.
pub fn has_cover_thumb(k: &mut KernelHandle<'_>, name_hash: u32) -> bool {
    let Some(hdr) = bundle::read_header(k.sd(), name_hash) else {
        return false;
    };
    hdr.has_flag(bundle::FLAG_COVERS_READY) && hdr.covers_size > 0
}

// ── filename-based convenience wrappers ─────────────────────────────

/// Load a cover thumbnail by book filename.
pub fn load_cover_for(k: &mut KernelHandle<'_>, filename: &[u8]) -> Option<DecodedImage> {
    let name_hash = cache::fnv1a(filename);
    load_cover_thumb(k, name_hash)
}

/// Check whether a cover thumbnail exists by book filename.
pub fn has_cover_for(k: &mut KernelHandle<'_>, filename: &[u8]) -> bool {
    let name_hash = cache::fnv1a(filename);
    has_cover_thumb(k, name_hash)
}
