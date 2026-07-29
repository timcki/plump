// FNV-1a hash.
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
}
