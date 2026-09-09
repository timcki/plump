// cover thumbnail helpers: read and write pre-dithered 1-bit covers
// inside the per-book bundle.
//
// the Covers section can hold N variants per book; the home screen
// today writes two: Card (112x160) for the CONTINUE card + Library
// grid, and Mini (64x96) for RECENT rows. layout:
//
//   [CoversHeader        12 bytes]
//   [CoverVariant[0..N]  16 bytes each]
//   [variant[i] bitmap data, packed in CoverVariant.data_offset order]
//
// the section is placed by `BundleFile` so it can't overlap PidxDir /
// Content (the v2 bug that silently zeroed cover bytes during the
// PIDX zero-fill).

use alloc::vec::Vec;

use plump_kernel::drivers::storage::FileReader;
use plump_kernel::kernel::bundle::{
    self, BundleError, BundleFile, BundleHeader, CoverKind, CoverVariant, CoversHeader, SectionId,
    SectionRange,
};
use plump_kernel::util::hash;

use crate::error::{Error, ErrorKind, Result};
use crate::kernel::KernelHandle;
use crate::kernel::work_queue::DecodedImage;

/// Card-thumb dimensions: home CONTINUE card, library grid cell.
pub const CARD_THUMB_W: u16 = 112;
pub const CARD_THUMB_H: u16 = 160;

/// Mini-thumb dimensions: home RECENT row.
pub const MINI_THUMB_W: u16 = 64;
pub const MINI_THUMB_H: u16 = 96;

/// A single variant being written to the bundle. The writer fills in
/// section-relative offsets; callers only supply the kind + image.
pub struct CoverVariantBlob<'a> {
    pub kind: CoverKind,
    pub image: &'a DecodedImage,
}

/// Persist `variants` to the bundle's Covers section. Reuses an
/// existing section when its size suffices; otherwise frees the old
/// range and allocates a fresh one at file tail (orphaning the old
/// bytes — acceptable churn on a 16 GB SD).
///
/// Writes all bitmaps before setting `FLAG_COVERS_READY` and
/// committing the header, so a power-loss mid-write leaves the
/// previous-known-good cover state in place.
pub fn save_cover_variants(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    variants: &[CoverVariantBlob<'_>],
) -> Result<()> {
    if variants.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "save_cover_variants: no variants",
        ));
    }
    if variants.len() > u8::MAX as usize {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "save_cover_variants: too many variants",
        ));
    }

    let mut bf = match BundleFile::open(k.sd(), name_hash) {
        Ok(bf) => bf,
        Err(BundleError::StaleVersion(v)) => {
            log::info!(
                "bundle: v{} stale on save_cover_variants, deleting {:#010X}",
                v,
                name_hash,
            );
            bundle::delete(k.sd(), name_hash)?;
            return Err(Error::new(
                ErrorKind::NotFound,
                "save_cover_variants: stale bundle",
            ));
        }
        Err(e) => return Err(e.into()),
    };

    let hdr_bytes = bundle::COVERS_HDR_SIZE as u32;
    let var_bytes = bundle::COVER_VARIANT_SIZE as u32;
    let mut total = hdr_bytes + var_bytes * variants.len() as u32;
    for v in variants {
        total = total.saturating_add(v.image.data.len() as u32);
    }

    // reuse-or-reallocate. when the existing section is too small we
    // clear the range from the header, commit so the on-disk header
    // doesn't reference the about-to-be-orphaned bytes, then allocate
    // a fresh section at the file tail.
    //
    // the inner block scopes the `Section` borrow on `bf` so we can
    // re-borrow `bf` mutably for the header flag + commit below
    // without an explicit `drop`.
    {
        let mut section = match bf.section(SectionId::Covers) {
            Some(range) if range.size >= total => bf.section_mut(SectionId::Covers)?,
            Some(_) => {
                bf.header_mut()
                    .set_section(SectionId::Covers, SectionRange::EMPTY);
                bf.commit_header()?;
                bf.allocate(SectionId::Covers, total)?
            }
            None => bf.allocate(SectionId::Covers, total)?,
        };

        let covers_hdr = CoversHeader {
            variant_count: variants.len() as u8,
            raw_format: bundle::RAW_FMT_NONE,
            raw_offset: 0,
            raw_size: 0,
        };
        section.write_at(0, &covers_hdr.encode())?;

        // bitmap region starts immediately after the variant entry table.
        let bitmaps_base = hdr_bytes + var_bytes * variants.len() as u32;
        let mut data_cursor = bitmaps_base;
        for (slot_idx, blob) in variants.iter().enumerate() {
            let bitmap_len = blob.image.data.len() as u32;
            let entry = CoverVariant {
                kind: blob.kind,
                width: blob.image.width,
                height: blob.image.height,
                stride: blob.image.stride as u16,
                data_offset: data_cursor,
                data_size: bitmap_len,
            };
            let entry_rel = hdr_bytes + var_bytes * slot_idx as u32;
            section.write_at(entry_rel, &entry.encode())?;
            section.write_at(data_cursor, &blob.image.data)?;
            data_cursor = data_cursor.saturating_add(bitmap_len);
        }
    }

    bf.header_mut().set_flag(bundle::FLAG_COVERS_READY, true);
    bf.commit_header()?;
    Ok(())
}

// variant table read in one go with the covers header. writers store
// two variants today (Card + Mini); entries past the fourth are not
// consulted.
const MAX_VARIANTS: usize = 4;
const TABLE_MAX: usize = bundle::COVERS_HDR_SIZE + MAX_VARIANTS * bundle::COVER_VARIANT_SIZE;

struct VariantTable {
    range: SectionRange,
    buf: [u8; TABLE_MAX],
    len: usize,
    count: usize,
}

impl VariantTable {
    fn entries(&self) -> impl Iterator<Item = CoverVariant> + '_ {
        (0..self.count).filter_map(move |i| {
            let start = bundle::COVERS_HDR_SIZE + i * bundle::COVER_VARIANT_SIZE;
            let end = start + bundle::COVER_VARIANT_SIZE;
            if end > self.len {
                return None;
            }
            CoverVariant::decode(&self.buf[start..end])
        })
    }

    /// The variant of kind `prefer`, else the first one of any kind.
    fn pick(&self, prefer: CoverKind) -> Option<CoverVariant> {
        let mut chosen = None;
        for v in self.entries() {
            if v.kind == prefer {
                return Some(v);
            }
            if chosen.is_none() {
                chosen = Some(v);
            }
        }
        chosen
    }
}

/// Covers header plus variant table of an open bundle, fetched with a
/// single read. None when covers aren't ready or the section is short.
fn variant_table_in(r: &mut FileReader<'_>, hdr: &BundleHeader) -> Option<VariantTable> {
    if !hdr.has_flag(bundle::FLAG_COVERS_READY) {
        return None;
    }
    let range = hdr.section(SectionId::Covers)?;
    if range.size < (bundle::COVERS_HDR_SIZE + bundle::COVER_VARIANT_SIZE) as u32 {
        return None;
    }
    let mut buf = [0u8; TABLE_MAX];
    let want = (range.size as usize).min(TABLE_MAX);
    let len = r.read_at(range.offset, &mut buf[..want]).ok()?;
    if len < bundle::COVERS_HDR_SIZE {
        return None;
    }
    let covers_hdr = CoversHeader::decode(&buf[..bundle::COVERS_HDR_SIZE])?;
    Some(VariantTable {
        range,
        buf,
        len,
        count: covers_hdr.variant_count as usize,
    })
}

/// The `prefer` variant (or the first available) of an open bundle:
/// one read for the table, one for the bitmap.
fn read_cover_in(
    r: &mut FileReader<'_>,
    hdr: &BundleHeader,
    prefer: CoverKind,
) -> Option<DecodedImage> {
    let table = variant_table_in(r, hdr)?;
    let variant = table.pick(prefer)?;
    if variant.width == 0 || variant.height == 0 {
        return None;
    }
    // the bitmap must lie inside the recorded section
    let data_end = variant.data_offset.checked_add(variant.data_size)?;
    if data_end > table.range.size {
        return None;
    }

    let data_len = variant.data_size as usize;
    let mut data = Vec::new();
    data.try_reserve_exact(data_len).ok()?;
    data.resize(data_len, 0);
    let n = r
        .read_at(table.range.offset + variant.data_offset, &mut data)
        .ok()?;
    if n < data_len {
        return None;
    }

    Some(DecodedImage {
        width: variant.width,
        height: variant.height,
        data,
        stride: variant.stride as usize,
    })
}

/// Read the variant matching `prefer`; falls back to the first
/// available variant of any kind when the preferred one is missing.
pub fn load_cover_variant(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    prefer: CoverKind,
) -> Option<DecodedImage> {
    bundle::with_reader(k.sd(), name_hash, |r| {
        Ok(bundle::read_header_in(r).and_then(|hdr| read_cover_in(r, &hdr, prefer)))
    })
    .ok()
    .flatten()
}

/// True iff the bundle has a variant of the given kind.
pub fn has_cover_variant(k: &mut KernelHandle<'_>, name_hash: u32, kind: CoverKind) -> bool {
    bundle::with_reader(k.sd(), name_hash, |r| {
        Ok(bundle::read_header_in(r)
            .and_then(|hdr| variant_table_in(r, &hdr))
            .is_some_and(|table| table.entries().any(|v| v.kind == kind)))
    })
    .unwrap_or(false)
}

/// Everything a library row shows from the bundle, read in a single
/// file session: the mini cover and the cached page count.
#[derive(Default)]
pub struct LibraryEntry {
    pub cover: Option<DecodedImage>,
    /// None until the book has been opened and indexed at least once
    pub total_pages: Option<u32>,
}

pub fn load_library_entry(k: &mut KernelHandle<'_>, name_hash: u32) -> LibraryEntry {
    bundle::with_reader(k.sd(), name_hash, |r| {
        let Some(hdr) = bundle::read_header_in(r) else {
            return Ok(LibraryEntry::default());
        };
        Ok(LibraryEntry {
            cover: read_cover_in(r, &hdr, CoverKind::Mini),
            total_pages: bundle::total_pages_in(r, &hdr),
        })
    })
    .unwrap_or_default()
}

/// Filename-keyed wrapper around `load_cover_variant`.
pub fn load_cover_variant_for(
    k: &mut KernelHandle<'_>,
    filename: &[u8],
    prefer: CoverKind,
) -> Option<DecodedImage> {
    let name_hash = hash::fnv1a(filename);
    load_cover_variant(k, name_hash, prefer)
}
