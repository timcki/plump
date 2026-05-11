// FNV-1a hash + BookId newtype.
//
// the codebase historically had three copies of the same hash function:
// `kernel::bookmarks::fnv1a_icase` (case-folded, used by bookmarks),
// `smol_epub::cache::fnv1a` (case-sensitive, used by per-book bundles),
// and `smol_epub::cache::fnv1a_icase`. this module is the single source
// of truth for distro code; smol_epub keeps its own copies for crate
// independence but the rest of the firmware routes through here.
//
// note on case sensitivity: bookmarks use case-folded hashes
// (`fnv1a_icase`) because FAT filenames round-trip case in ways we
// can't predict. per-book bundles use case-sensitive hashes
// (`fnv1a`) because the bundle file name is `_<hash>.BIN` and we want
// it stable against the exact filename that opened the book. mixing
// the two would silently re-key persisted data; keep them separate.

/// Stable per-book identifier derived from the filename.
///
/// Wraps a `u32` so the hash flavour is documented at construction
/// (`from_filename` is case-sensitive; `from_filename_icase` matches
/// bookmark semantics). Methods that downstream want a raw `u32` can
/// call `raw`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BookId(pub u32);

impl BookId {
    pub const ZERO: Self = Self(0);

    /// Case-sensitive hash. Matches the historical bundle naming
    /// scheme (`smol_epub::cache::fnv1a`).
    #[inline]
    pub fn from_filename(name: &[u8]) -> Self {
        Self(fnv1a(name))
    }

    /// Case-folded hash. Matches bookmark lookup semantics.
    #[inline]
    pub fn from_filename_icase(name: &[u8]) -> Self {
        Self(fnv1a_icase(name))
    }

    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }
}

impl From<u32> for BookId {
    #[inline]
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

/// FNV-1a, case-sensitive.
#[inline]
pub fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// FNV-1a with ASCII case folding.
#[inline]
pub fn fnv1a_icase(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b.to_ascii_lowercase() as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_known_vectors() {
        // FNV-1a of "" is the offset basis.
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        // canonical FNV-1a test vector for "a"
        assert_eq!(fnv1a(b"a"), 0xe40c_292c);
    }

    #[test]
    fn icase_folds_case() {
        assert_eq!(fnv1a_icase(b"FOO.epub"), fnv1a_icase(b"foo.EPUB"));
        assert_ne!(fnv1a(b"FOO.epub"), fnv1a(b"foo.EPUB"));
    }

    #[test]
    fn book_id_constructors_differ_on_case() {
        assert_ne!(
            BookId::from_filename(b"Foo.epub"),
            BookId::from_filename(b"foo.epub")
        );
        assert_eq!(
            BookId::from_filename_icase(b"Foo.epub"),
            BookId::from_filename_icase(b"foo.epub")
        );
    }
}
