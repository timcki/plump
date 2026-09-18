//! Dictionary hyphenation for the words of a stripped chapter.
//!
//! A thin, `no_std` layer over [`hypher`] (Typst's Liang-pattern
//! tries) that speaks the reader's language: a word arrives as the
//! bytes of one [`crate::markup::Event::Word`], and what comes back
//! are the byte offsets inside it where a line may break with a
//! hyphen. Which words qualify (letters only, long enough) and the
//! per-language minimum fragments (`hypher`'s bounds) are decided
//! here, so every consumer hyphenates the same way.

pub use hypher::Lang;

/// Most break points reported for one word.
pub const MAX_BREAKS: usize = 15;
/// Words shorter than this (in characters) are never hyphenated.
pub const MIN_WORD_CHARS: usize = 5;
/// Words longer than this (in bytes) are never hyphenated.
pub const MAX_WORD_BYTES: usize = 64;

/// Resolve a `dc:language` value (`"en"`, `"en-US"`, `"pl-PL"`) to a
/// pattern set. `None` when the language is unknown or the crate was
/// built without its patterns.
pub fn lang_from_tag(tag: &[u8]) -> Option<Lang> {
    if tag.len() < 2 {
        return None;
    }
    let code = [tag[0].to_ascii_lowercase(), tag[1].to_ascii_lowercase()];
    if tag.len() > 2 && tag[2].is_ascii_alphabetic() {
        // three-letter codes are not ISO 639-1
        return None;
    }
    Lang::from_iso(code)
}

/// Break offsets for `word`, written to `out` in increasing order as
/// byte offsets from the start of the word; returns how many.
///
/// Zero for anything that is not a plain word of letters (digits,
/// apostrophes, explicit hyphens), for words under [`MIN_WORD_CHARS`]
/// or over [`MAX_WORD_BYTES`], and for invalid UTF-8. Case does not
/// matter: `hypher` folds it.
pub fn breaks(word: &[u8], lang: Lang, out: &mut [u16; MAX_BREAKS]) -> usize {
    if word.len() > MAX_WORD_BYTES {
        return 0;
    }
    let Ok(s) = core::str::from_utf8(word) else {
        return 0;
    };
    let mut chars = 0usize;
    for c in s.chars() {
        if !c.is_alphabetic() {
            return 0;
        }
        chars += 1;
    }
    if chars < MIN_WORD_CHARS {
        return 0;
    }

    let mut n = 0usize;
    let mut offset = 0usize;
    let mut parts = hypher::hyphenate(s, lang).peekable();
    while let Some(part) = parts.next() {
        offset += part.len();
        if parts.peek().is_none() {
            break;
        }
        if n == MAX_BREAKS {
            break;
        }
        out[n] = offset as u16;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn at(word: &str, lang: Lang) -> Vec<u16> {
        let mut out = [0u16; MAX_BREAKS];
        let n = breaks(word.as_bytes(), lang, &mut out);
        out[..n].to_vec()
    }

    #[test]
    fn english_breaks_at_syllables() {
        assert_eq!(at("assumptions", Lang::English), vec![2, 6]);
        assert_eq!(at("hyphenation", Lang::English), vec![2, 6]);
    }

    #[test]
    fn case_is_folded() {
        assert_eq!(at("Assumptions", Lang::English), at("assumptions", Lang::English));
    }

    #[test]
    fn polish_offsets_count_bytes_not_chars() {
        // przy|ja|ciół|ka: ó and ł are two bytes each, so the last break
        // lands at byte 12, not char 10
        assert_eq!(at("przyjaciółka", Lang::Polish), vec![4, 6, 12]);
    }

    #[test]
    fn short_words_and_non_letters_are_left_alone() {
        assert!(at("word", Lang::English).is_empty());
        assert!(at("don't", Lang::English).is_empty());
        assert!(at("well-known", Lang::English).is_empty());
        assert!(at("abc123def", Lang::English).is_empty());
    }

    #[test]
    fn invalid_utf8_and_overlong_words_are_skipped() {
        let mut out = [0u16; MAX_BREAKS];
        assert_eq!(breaks(b"ab\xFFcdef", Lang::English, &mut out), 0);
        let long = [b'a'; MAX_WORD_BYTES + 1];
        assert_eq!(breaks(&long, Lang::English, &mut out), 0);
    }

    #[test]
    fn language_tags() {
        assert_eq!(lang_from_tag(b"en"), Some(Lang::English));
        assert_eq!(lang_from_tag(b"en-US"), Some(Lang::English));
        assert_eq!(lang_from_tag(b"PL"), Some(Lang::Polish));
        assert_eq!(lang_from_tag(b"eng"), None);
        assert_eq!(lang_from_tag(b"x"), None);
        // a language whose patterns are not compiled in
        assert_eq!(lang_from_tag(b"de"), None);
    }
}
