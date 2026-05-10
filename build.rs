fn main() {
    linker_be_nice();
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    generate_bitmap_fonts();
}

fn hint(msg: &str) {
    eprintln!();
    eprintln!("hint: {msg}");
    eprintln!();
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    // --error-handling-script passes two args: kind and symbol
    if args.len() >= 3 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => hint(
                    "`defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`",
                ),
                "_stack_start" => hint("is the linker script `linkall.x` missing?"),
                what if what.starts_with("esp_rtos_") => hint(
                    "`esp-radio` has no scheduler enabled. make sure you have initialized `esp-rtos` or provided an external scheduler.",
                ),
                "embedded_test_linker_file_not_added_to_rustflags" => hint(
                    "`embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests",
                ),
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => hint(
                    "did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?",
                ),
                _ => (),
            },
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}

// build-time font rasterisation: scan assets/fonts/<Family>/ for TTFs,
// classify by weight/style, rasterise to 2-bit (4-level) bitmaps, emit
// font_data.rs.
//
// per-family subdirectories (e.g. assets/fonts/Bookerly/, .../Atkinson/,
// .../Inter/). filename matching is hyphen-token strict so Inter's many
// weight variants (SemiBold, ExtraBold, Black, ...) don't collide with
// the canonical Bold/Italic.
//
// five size tiers: XSmall / Small / Medium / Large / XLarge
// two glyph sets per font:
//   ascii 0x20-0x7E (contiguous direct-indexed table)
//   extended unicode (sorted codepoint table, binary-searched at runtime)
//
// extended set covers latin-1 supplement, common punctuation (smart
// quotes, dashes, ellipsis, bullet), and a handful of currency/math
// symbols, enough for the vast majority of european-language epubs.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// font families to rasterise. tuple is (constant prefix, subdirectory).
// Bookerly + Atkinson supply reader fonts (selectable); Inter supplies
// every UI surface (chrome, menus, status bars, button labels).
const FAMILIES: &[(&str, &str)] = &[
    ("BOOKERLY", "Bookerly"),
    ("ATKINSON", "Atkinson"),
    ("INTER", "Inter"),
];

// body sizes (px): 0=XSmall 1=Small 2=Medium 3=Large 4=XLarge
const BODY_PX: [(f32, &str); 5] = [
    (16.0, "XSMALL"),
    (19.0, "SMALL"),
    (23.0, "MEDIUM"),
    (28.0, "LARGE"),
    (35.0, "XLARGE"),
];

// Heading sizes scale proportionally
const HEADING_PX: [(f32, &str); 5] = [
    (23.0, "XSMALL"),
    (27.0, "SMALL"),
    (32.0, "MEDIUM"),
    (38.0, "LARGE"),
    (46.0, "XLARGE"),
];

// ASCII range (direct-indexed)
const FIRST_CHAR: u8 = 0x20;
const LAST_CHAR: u8 = 0x7E;
const GLYPH_COUNT: usize = (LAST_CHAR - FIRST_CHAR + 1) as usize;

// build the sorted list of extended unicode codepoints to rasterise
fn extended_codepoints() -> Vec<u32> {
    let mut cps: Vec<u32> = Vec::new();

    // latin-1 supplement (0x00A0-0x00FF): accented letters, symbols.
    // NBSP (U+00A0) is included: in every shipped reader font it
    // rasterises to a zero-ink glyph whose advance matches the ASCII
    // space, which is exactly what K-P measures it as in items.rs
    // (advance(' ', style)). No special-case needed at the draw site.
    // SHY (U+00AD) is excluded: fonts ship it as a visible
    // discretionary-hyphen glyph with a positive advance (~2× space)
    // and a small bitmap. K-P models SHY as a zero-width Penalty
    // (items.rs:280), so resolving SHY through the font would both
    // overflow lines and draw spurious hyphens at every syllable
    // boundary. The draw loop in src/apps/reader/mod.rs skips SHY
    // bytes outright. Visible-hyphen rendering at a chosen break is
    // a separate concern (deferred per items.rs comment).
    for cp in 0x00A0..=0x00FFu32 {
        if cp == 0x00AD {
            continue;
        }
        cps.push(cp);
    }

    // Latin Extended-A: most common characters in European languages
    // Czech, Polish, Hungarian, Turkish, Romanian, etc.
    let latin_ext_a: &[u32] = &[
        0x0100, 0x0101, // Āā
        0x0102, 0x0103, // Ăă
        0x0104, 0x0105, // Ąą
        0x0106, 0x0107, // Ćć
        0x010C, 0x010D, // Čč
        0x010E, 0x010F, // Ďď
        0x0110, 0x0111, // Đđ
        0x0118, 0x0119, // Ęę
        0x011A, 0x011B, // Ěě
        0x011E, 0x011F, // Ğğ
        0x0130, 0x0131, // İı
        0x0141, 0x0142, // Łł
        0x0143, 0x0144, // Ńń
        0x0147, 0x0148, // Ňň
        0x0150, 0x0151, // Őő
        0x0152, 0x0153, // Œœ
        0x0158, 0x0159, // Řř
        0x015A, 0x015B, // Śś
        0x015E, 0x015F, // Şş
        0x0160, 0x0161, // Šš
        0x0162, 0x0163, // Ţţ
        0x0164, 0x0165, // Ťť
        0x016E, 0x016F, // Ůů
        0x0170, 0x0171, // Űű
        0x0178, // Ÿ
        0x0179, 0x017A, // Źź
        0x017B, 0x017C, // Żż
        0x017D, 0x017E, // Žž
    ];
    cps.extend_from_slice(latin_ext_a);

    // General Punctuation: hyphens, dashes, quotes, ellipsis, bullet
    let punctuation: &[u32] = &[
        0x2010, // ‐ hyphen
        0x2011, // ‑ non-breaking hyphen
        0x2012, // ‒ figure dash
        0x2013, // – en dash
        0x2014, // — em dash
        0x2015, // ― horizontal bar
        0x2018, // ' left single quotation mark
        0x2019, // ' right single quotation mark
        0x201A, // ‚ single low-9 quotation mark
        0x201B, // ‛ single high-reversed-9
        0x201C, // " left double quotation mark
        0x201D, // " right double quotation mark
        0x201E, // „ double low-9 quotation mark
        0x201F, // ‟ double high-reversed-9
        0x2022, // • bullet
        0x2026, // … horizontal ellipsis
        0x2032, // ′ prime
        0x2033, // ″ double prime
        0x2039, // ‹ single left-pointing angle quotation
        0x203A, // › single right-pointing angle quotation
    ];
    cps.extend_from_slice(punctuation);

    // Currency symbols (common ones for e-books)
    let currency: &[u32] = &[
        0x00A2, // ¢ cent sign (already in Latin-1, but explicit)
        0x00A3, // £ pound sign (already in Latin-1)
        0x00A5, // ¥ yen sign (already in Latin-1)
        0x20AC, // € euro sign
        0x20A3, // ₣ french franc
        0x20A4, // ₤ lira sign
        0x20A7, // ₧ peseta sign
        0x20A9, // ₩ won sign
        0x20B9, // ₹ indian rupee
        0x20BD, // ₽ ruble sign
    ];
    cps.extend_from_slice(currency);

    // Math symbols (common in books)
    let math: &[u32] = &[
        0x00B1, // ± plus-minus (already in Latin-1)
        0x00D7, // × multiplication (already in Latin-1)
        0x00F7, // ÷ division (already in Latin-1)
        0x2212, // − minus sign
        0x2260, // ≠ not equal to
        0x2264, // ≤ less than or equal to
        0x2265, // ≥ greater than or equal to
        0x2248, // ≈ almost equal to
        0x221E, // ∞ infinity
        0x221A, // √ square root
        0x03C0, // π pi (Greek letter)
        0x00B0, // ° degree sign (already in Latin-1)
        0x2030, // ‰ per mille sign
        0x2070, // ⁰ superscript 0
        0x00B9, // ¹ superscript 1 (already in Latin-1)
        0x00B2, // ² superscript 2 (already in Latin-1)
        0x00B3, // ³ superscript 3 (already in Latin-1)
        0x2074, // ⁴ superscript 4
        0x2075, // ⁵ superscript 5
        0x2076, // ⁶ superscript 6
        0x2077, // ⁷ superscript 7
        0x2078, // ⁸ superscript 8
        0x2079, // ⁹ superscript 9
    ];
    cps.extend_from_slice(math);

    // Arrows (useful for navigation hints, diagrams)
    let arrows: &[u32] = &[
        0x2190, // ← leftwards arrow
        0x2191, // ↑ upwards arrow
        0x2192, // → rightwards arrow
        0x2193, // ↓ downwards arrow
        0x2194, // ↔ left right arrow
        0x2195, // ↕ up down arrow
        0x21D0, // ⇐ leftwards double arrow
        0x21D2, // ⇒ rightwards double arrow
        0x21D4, // ⇔ left right double arrow
    ];
    cps.extend_from_slice(arrows);

    // Miscellaneous symbols (common in e-books)
    let misc: &[u32] = &[
        0x2122, // ™ trade mark sign
        0x00A9, // © copyright (already in Latin-1)
        0x00AE, // ® registered (already in Latin-1)
        0x2020, // † dagger
        0x2021, // ‡ double dagger
        0x2023, // ‣ triangular bullet
        0x25A0, // ■ black square
        0x25A1, // □ white square
        0x25CF, // ● black circle
        0x25CB, // ○ white circle
        0x2605, // ★ black star
        0x2606, // ☆ white star
        0x2713, // ✓ check mark
        0x2717, // ✗ ballot x
        0x00A7, // § section sign (already in Latin-1)
        0x00B6, // ¶ pilcrow / paragraph sign (already in Latin-1)
    ];
    cps.extend_from_slice(misc);

    // Additional Latin Extended-B for more complete European language support
    let latin_ext_b: &[u32] = &[
        0x0180, // ƀ b with stroke (Croatian)
        0x0192, // ƒ f with hook (Dutch florin)
        0x01A0, 0x01A1, // Ơơ O with horn (Vietnamese)
        0x01AF, 0x01B0, // Ưư U with horn (Vietnamese)
        0x01CD, 0x01CE, // Ǎǎ A with caron
        0x01CF, 0x01D0, // Ǐǐ I with caron
        0x01D1, 0x01D2, // Ǒǒ O with caron
        0x01D3, 0x01D4, // Ǔǔ U with caron
        0x0218, 0x0219, // Șș S with comma below (Romanian)
        0x021A, 0x021B, // Țț T with comma below (Romanian)
    ];
    cps.extend_from_slice(latin_ext_b);

    cps.sort();
    cps.dedup();
    cps
}

// find first .ttf in dir whose name contains all keywords (case-insensitive);
// excludes BoldItalic unless explicitly requested
fn find_ttf(dir: &Path, keyword: &str) -> Option<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|e| e.eq_ignore_ascii_case("ttf"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();

    // require the keyword to appear as the LAST hyphen-delimited token
    // in the file stem. this avoids matching SemiBold / ExtraBold /
    // BoldItalic when looking for just "Bold" — important for Inter,
    // which ships ~9 weight variants.
    for path in &candidates {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        if let Some(last) = stem.rsplit('-').next() {
            if last.eq_ignore_ascii_case(keyword) {
                return Some(path.clone());
            }
        }
    }
    None
}

fn generate_bitmap_fonts() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("font_data.rs");
    let mut out = fs::File::create(&dest).unwrap();

    let assets = Path::new("assets/fonts");
    println!("cargo:rerun-if-changed=assets/fonts");

    let ext_codepoints = extended_codepoints();

    // header
    writeln!(out, "// AUTO-GENERATED by build.rs - do not edit").unwrap();
    writeln!(
        out,
        "use crate::fonts::bitmap::{{BitmapFont, BitmapGlyph, GLYPH_COUNT}};"
    )
    .unwrap();
    writeln!(out).unwrap();

    // single global emission: every family shares the same extended set
    writeln!(
        out,
        "pub const EXT_GLYPH_COUNT: usize = {};",
        ext_codepoints.len()
    )
    .unwrap();
    writeln!(out).unwrap();

    for (prefix, dirname) in FAMILIES {
        let dir = assets.join(dirname);
        println!("cargo:rerun-if-changed={}", dir.display());

        let regular = find_ttf(&dir, "Regular");
        let bold = find_ttf(&dir, "Bold");
        let italic = find_ttf(&dir, "Italic");

        if let Some(ref p) = regular {
            println!("cargo:rerun-if-changed={}", p.display());
        }
        if let Some(ref p) = bold {
            println!("cargo:rerun-if-changed={}", p.display());
        }
        if let Some(ref p) = italic {
            println!("cargo:rerun-if-changed={}", p.display());
        }

        writeln!(
            out,
            "pub const {prefix}_HAS_REGULAR: bool = {};",
            regular.is_some()
        )
        .unwrap();

        // regular: body + heading sizes
        emit_or_stub_set(
            &mut out,
            regular.as_deref(),
            prefix,
            "REGULAR",
            true,
            &ext_codepoints,
        );
        // bold + italic: body sizes only (headings always use regular)
        emit_or_stub_set(
            &mut out,
            bold.as_deref(),
            prefix,
            "BOLD",
            false,
            &ext_codepoints,
        );
        emit_or_stub_set(
            &mut out,
            italic.as_deref(),
            prefix,
            "ITALIC",
            false,
            &ext_codepoints,
        );
    }
}

fn emit_or_stub_set(
    out: &mut fs::File,
    path: Option<&Path>,
    family_prefix: &str,
    style_prefix: &str,
    include_heading: bool,
    ext_codepoints: &[u32],
) {
    if let Some(p) = path {
        let data = fs::read(p).unwrap();
        let font = fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
            .unwrap_or_else(|_| panic!("failed to parse {}", p.display()));
        eprintln!(
            "cargo:warning=font: rasterising {} as {family_prefix}_{style_prefix}",
            p.file_name().unwrap().to_string_lossy(),
        );
        for (px, suffix) in &BODY_PX {
            emit_font(
                out,
                &font,
                &format!("{family_prefix}_{style_prefix}_BODY_{suffix}"),
                *px,
                ext_codepoints,
            );
        }
        if include_heading {
            for (px, suffix) in &HEADING_PX {
                emit_font(
                    out,
                    &font,
                    &format!("{family_prefix}_{style_prefix}_HEADING_{suffix}"),
                    *px,
                    ext_codepoints,
                );
            }
        }
    } else {
        for (_px, suffix) in &BODY_PX {
            emit_stub(out, &format!("{family_prefix}_{style_prefix}_BODY_{suffix}"));
        }
        if include_heading {
            for (_px, suffix) in &HEADING_PX {
                emit_stub(
                    out,
                    &format!("{family_prefix}_{style_prefix}_HEADING_{suffix}"),
                );
            }
        }
    }
}

struct RasterGlyph {
    advance: u8,
    offset_x: i8,
    offset_y: i8,
    width: u8,
    height: u8,
    bits: Vec<u8>,
}

fn rasterize_char(font: &fontdue::Font, ch: char, px: f32) -> RasterGlyph {
    let (metrics, coverage) = font.rasterize(ch, px);
    let w = metrics.width;
    let h = metrics.height;
    let row_bytes = w.div_ceil(4); // 4 pixels per byte at 2bpp

    // pack coverage to 2-bit MSB-first (4 pixels per byte)
    // pixel 0 in bits 7:6, pixel 1 in bits 5:4, pixel 2 in bits 3:2, pixel 3 in bits 1:0
    // coverage quantization: <64 → 0 (white), <128 → 1 (light gray),
    //                        <192 → 2 (dark gray), >=192 → 3 (black)
    let mut bits = Vec::with_capacity(row_bytes * h);
    for y in 0..h {
        for bx in 0..row_bytes {
            let mut byte = 0u8;
            for px_idx in 0..4usize {
                let x = bx * 4 + px_idx;
                if x < w {
                    let c = coverage[y * w + x];
                    let val: u8 = if c < 64 {
                        0
                    } else if c < 128 {
                        1
                    } else if c < 192 {
                        2
                    } else {
                        3
                    };
                    byte |= val << (6 - px_idx * 2);
                }
            }
            bits.push(byte);
        }
    }

    // offset_y: baseline to top row (y-down screen space).
    // fontdue ymin = baseline to bottom edge; top = ymin+h above baseline;
    // negate for screen coords.
    let offset_y = -metrics.ymin - (h as i32);

    RasterGlyph {
        advance: (metrics.advance_width + 0.5) as u8,
        offset_x: (metrics.xmin).clamp(-128, 127) as i8,
        offset_y: offset_y.clamp(-128, 127) as i8,
        width: w.min(255) as u8,
        height: h.min(255) as u8,
        bits,
    }
}

fn emit_font(
    out: &mut fs::File,
    font: &fontdue::Font,
    name: &str,
    px: f32,
    ext_codepoints: &[u32],
) {
    // line metrics
    let lm = font
        .horizontal_line_metrics(px)
        .expect("font has no horizontal metrics");
    let line_height = lm.new_line_size.ceil() as u16;
    let ascent = lm.ascent.ceil() as u16;

    // ascii glyphs (direct-indexed 0x20-0x7E)

    let mut ascii_glyphs: Vec<RasterGlyph> = Vec::with_capacity(GLYPH_COUNT);
    let mut ascii_bits_total: usize = 0;

    for code in FIRST_CHAR..=LAST_CHAR {
        let g = rasterize_char(font, code as char, px);
        ascii_bits_total += g.bits.len();
        ascii_glyphs.push(g);
    }

    // emit ASCII glyph table
    writeln!(out, "static {name}_GLYPHS: [BitmapGlyph; GLYPH_COUNT] = [").unwrap();
    let mut offset: u16 = 0;
    for (i, g) in ascii_glyphs.iter().enumerate() {
        let ch = (FIRST_CHAR + i as u8) as char;
        writeln!(
            out,
            "    BitmapGlyph {{ advance: {:>2}, offset_x: {:>3}, offset_y: {:>4}, width: {:>2}, height: {:>2}, bitmap_offset: {:>5} }}, // {:?}",
            g.advance, g.offset_x, g.offset_y, g.width, g.height, offset, ch
        ).unwrap();
        offset += g.bits.len() as u16;
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    // emit ASCII bitmap data
    writeln!(out, "static {name}_BITMAPS: [u8; {ascii_bits_total}] = [").unwrap();
    emit_bitmap_bytes(out, &ascii_glyphs);
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    // extended unicode glyphs (sorted by codepoint)

    let mut ext_glyphs: Vec<RasterGlyph> = Vec::with_capacity(ext_codepoints.len());
    let mut ext_bits_total: usize = 0;

    for &cp in ext_codepoints {
        if let Some(ch) = char::from_u32(cp) {
            let g = rasterize_char(font, ch, px);
            ext_bits_total += g.bits.len();
            ext_glyphs.push(g);
        } else {
            // invalid codepoint, push a zero-width space placeholder
            ext_glyphs.push(RasterGlyph {
                advance: 0,
                offset_x: 0,
                offset_y: 0,
                width: 0,
                height: 0,
                bits: Vec::new(),
            });
        }
    }

    let ext_count = ext_codepoints.len();

    // emit extended codepoint lookup array
    writeln!(out, "static {name}_EXT_CP: [u32; {ext_count}] = [").unwrap();
    let mut col = 0;
    for &cp in ext_codepoints {
        if col == 0 {
            write!(out, "    ").unwrap();
        }
        write!(out, "0x{cp:04X},").unwrap();
        col += 1;
        if col >= 10 {
            writeln!(out).unwrap();
            col = 0;
        }
    }
    if col > 0 {
        writeln!(out).unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    // emit extended glyph table
    writeln!(
        out,
        "static {name}_EXT_GLYPHS: [BitmapGlyph; {ext_count}] = ["
    )
    .unwrap();
    let mut offset: u16 = 0;
    for (i, g) in ext_glyphs.iter().enumerate() {
        let cp = ext_codepoints[i];
        let ch_display = char::from_u32(cp)
            .map(|c| format!("{:?}", c))
            .unwrap_or_else(|| format!("U+{cp:04X}"));
        writeln!(
            out,
            "    BitmapGlyph {{ advance: {:>2}, offset_x: {:>3}, offset_y: {:>4}, width: {:>2}, height: {:>2}, bitmap_offset: {:>5} }}, // {}",
            g.advance, g.offset_x, g.offset_y, g.width, g.height, offset, ch_display
        ).unwrap();
        offset += g.bits.len() as u16;
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    // emit extended bitmap data
    writeln!(out, "static {name}_EXT_BITMAPS: [u8; {ext_bits_total}] = [").unwrap();
    emit_bitmap_bytes(out, &ext_glyphs);
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    // BitmapFont struct

    writeln!(out, "pub static {name}: BitmapFont = BitmapFont {{").unwrap();
    writeln!(out, "    glyphs: &{name}_GLYPHS,").unwrap();
    writeln!(out, "    bitmaps: &{name}_BITMAPS,").unwrap();
    writeln!(out, "    ext_codepoints: &{name}_EXT_CP,").unwrap();
    writeln!(out, "    ext_glyphs: &{name}_EXT_GLYPHS,").unwrap();
    writeln!(out, "    ext_bitmaps: &{name}_EXT_BITMAPS,").unwrap();
    writeln!(out, "    line_height: {line_height},").unwrap();
    writeln!(out, "    ascent: {ascent},").unwrap();
    writeln!(out, "}};").unwrap();
    writeln!(out).unwrap();
}

fn emit_stub(out: &mut fs::File, name: &str) {
    writeln!(
        out,
        "pub static {name}: BitmapFont = BitmapFont {{\
         glyphs: &[BitmapGlyph {{ advance: 0, offset_x: 0, offset_y: 0, width: 0, height: 0, bitmap_offset: 0 }}; GLYPH_COUNT], \
         bitmaps: &[], \
         ext_codepoints: &[], \
         ext_glyphs: &[], \
         ext_bitmaps: &[], \
         line_height: 13, \
         ascent: 13 \
         }};"
    )
    .unwrap();
    writeln!(out).unwrap();
}

fn emit_bitmap_bytes(out: &mut fs::File, glyphs: &[RasterGlyph]) {
    let mut col = 0;
    for g in glyphs {
        for &b in &g.bits {
            if col == 0 {
                write!(out, "    ").unwrap();
            }
            write!(out, "0x{b:02X},").unwrap();
            col += 1;
            if col >= 16 {
                writeln!(out).unwrap();
                col = 0;
            }
        }
    }
    if col > 0 {
        writeln!(out).unwrap();
    }
}
