// cover thumbnail helpers: read and write pre-dithered 1-bit covers
// inside the per-book bundle.
//
// v3 stores a single `Card` variant in the `Covers` section managed by
// `bundle::BundleFile`. Phase 5 will extend the section with extra
// sizes (tiny, small, detail) and the raw source bytes.
//
// the covers section layout is unchanged from v2:
//
//   [CoversHeader      12 bytes]
//   [CoverVariant[0]   16 bytes per entry]  variant_count entries
//   [variant[0] bitmap data]                1-bit packed, stride = w/8
//   ...
//
// The placement of the section is now under `BundleFile` control,
// which guarantees it can't overlap PidxDir / Content (the v2 bug
// that silently zeroed cover bytes on every PIDX zero-fill).

use alloc::vec::Vec;

use smol_epub::cache;

use plump_kernel::kernel::bundle;
use plump_kernel::kernel::bundle::{BundleError, BundleFile, SectionId};

use crate::kernel::KernelHandle;
use crate::kernel::work_queue::DecodedImage;

/// Maximum width for the Card cover variant (fits the home-screen card).
pub const COVER_THUMB_MAX_W: u16 = 200;

/// Maximum height for the Card cover variant.
pub const COVER_THUMB_MAX_H: u16 = 240;

/// Write the Card variant of a cover for the given book (by filename).
///
/// The bundle must exist (chapter caching initializes it).
pub fn save_cover_thumb_for(
    k: &mut KernelHandle<'_>,
    filename: &[u8],
    img: &DecodedImage,
) -> crate::error::Result<()> {
    let name_hash = cache::fnv1a(filename);
    save_cover_thumb(k, name_hash, img)
}

/// Write the Card variant of a cover into the bundle identified by
/// `name_hash`. On regeneration the section is reused in place when
/// the new bytes fit; otherwise a fresh `Covers` section is allocated
/// at the bundle tail (only valid after `PidxDir` exists, enforced by
/// `BundleFile::allocate`).
pub fn save_cover_thumb(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    img: &DecodedImage,
) -> crate::error::Result<()> {
    let mut bf = match BundleFile::open(k.sd(), name_hash) {
        Ok(bf) => bf,
        Err(BundleError::StaleVersion(v)) => {
            log::info!(
                "bundle: v{} stale on save_cover_thumb, deleting {:#010X}",
                v,
                name_hash,
            );
            bundle::delete(k.sd(), name_hash)?;
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::NotFound,
                "save_cover_thumb: stale bundle",
            ));
        }
        Err(e) => return Err(e.into()),
    };

    // section layout: CoversHeader + 1 variant entry + bitmap data
    let hdr_bytes = bundle::COVERS_HDR_SIZE as u32;
    let var_bytes = bundle::COVER_VARIANT_SIZE as u32;
    let data_bytes = img.data.len() as u32;
    let section_size = hdr_bytes + var_bytes + data_bytes;

    // Existing `Covers` section: reuse iff the new bytes fit in the
    // already-recorded range. We deliberately don't widen the recorded
    // size in place — that would invalidate the layout invariant for
    // neighbouring sections (e.g. PidxData), and BundleFile would
    // reject the resulting overlap on `commit_header` anyway.
    let existing = bf.section(SectionId::Covers);
    let mut section = match existing {
        Some(range) if range.size >= section_size => bf.section_mut(SectionId::Covers)?,
        Some(_) => {
            // Existing section is too small for the new bitmap. Today
            // this shouldn't happen (one variant, fixed thumb size),
            // but if it does we surface a clear error rather than
            // silently overflowing.
            log::warn!(
                "save_cover_thumb: existing Covers section too small; refusing to grow"
            );
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::InvalidData,
                "cover: section size shrunk",
            ));
        }
        None => bf.allocate(SectionId::Covers, section_size)?,
    };

    // write CoversHeader at offset 0 of the section
    let covers_hdr = bundle::CoversHeader {
        variant_count: 1,
        raw_format: bundle::RAW_FMT_NONE,
        raw_offset: 0,
        raw_size: 0,
    };
    section.write_at(0, &covers_hdr.encode())?;

    // write variant entry at offset COVERS_HDR_SIZE
    let data_offset_in_section = hdr_bytes + var_bytes;
    let variant = bundle::CoverVariant {
        kind: bundle::COVER_KIND_CARD,
        width: img.width,
        height: img.height,
        stride: img.stride as u16,
        data_offset: data_offset_in_section,
        data_size: data_bytes,
    };
    section.write_at(hdr_bytes, &variant.encode())?;

    // write bitmap bytes — `Section::write_at` bounds check ensures
    // we cannot punch past the recorded size (the v2 bug surface).
    section.write_at(data_offset_in_section, &img.data)?;
    drop(section);

    bf.header_mut().set_flag(bundle::FLAG_COVERS_READY, true);
    bf.commit_header()?;

    Ok(())
}

/// Load the Card variant (only variant in v1) from the bundle.
pub fn load_cover_thumb(k: &mut KernelHandle<'_>, name_hash: u32) -> Option<DecodedImage> {
    let mut bf = BundleFile::open(k.sd(), name_hash).ok()?;
    if !bf.header().has_flag(bundle::FLAG_COVERS_READY) {
        return None;
    }
    let covers_range = bf.section(SectionId::Covers)?;
    if covers_range.size < (bundle::COVERS_HDR_SIZE + bundle::COVER_VARIANT_SIZE) as u32 {
        return None;
    }

    let mut covers_hdr_buf = [0u8; bundle::COVERS_HDR_SIZE];
    let section = bf.section_mut(SectionId::Covers).ok()?;
    section.read_at(0, &mut covers_hdr_buf).ok()?;
    let covers_hdr = bundle::CoversHeader::decode(&covers_hdr_buf)?;
    if covers_hdr.variant_count == 0 {
        return None;
    }

    // pick the first variant whose kind is Card; fall back to variant 0
    let mut var_buf = [0u8; bundle::COVER_VARIANT_SIZE];
    let mut chosen: Option<bundle::CoverVariant> = None;
    for i in 0..covers_hdr.variant_count as u32 {
        let rel = bundle::COVERS_HDR_SIZE as u32 + i * bundle::COVER_VARIANT_SIZE as u32;
        section.read_at(rel, &mut var_buf).ok()?;
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
    section.read_at(variant.data_offset, &mut data).ok()?;

    Some(DecodedImage {
        width: variant.width,
        height: variant.height,
        data,
        stride: variant.stride as usize,
    })
}

/// Whether a cover has been generated for this book.
pub fn has_cover_thumb(k: &mut KernelHandle<'_>, name_hash: u32) -> bool {
    let Ok(bf) = BundleFile::open(k.sd(), name_hash) else {
        return false;
    };
    bf.header().has_flag(bundle::FLAG_COVERS_READY) && bf.section(SectionId::Covers).is_some()
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
