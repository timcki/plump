use std::{collections::BTreeMap, fs, io::Write, path::Path};

use freetype::{Face, Library, RenderMode, bitmap::PixelMode, face::LoadFlag};
use image::{GrayImage, Luma};

// reuse the actual fontdue rasterizer, family ladders, file selection and
// GPOS extraction rather than maintaining an approximation of the baseline.
#[allow(dead_code)]
mod production {
    include!("../../../build.rs");

    pub fn families() -> Vec<(&'static str, [f32; 2])> {
        FAMILIES
            .iter()
            .filter(|f| f.kern)
            .map(|f| (f.dir, [f.body[1], f.body[2]]))
            .collect()
    }

    pub fn path(root: &Path, family: &str, style: &str) -> PathBuf {
        find_ttf(&root.join("assets/fonts").join(family), style).expect("missing font")
    }

    pub fn glyph(font: &fontdue::Font, ch: char, px: f32) -> super::Glyph {
        let g = rasterize_char(font, ch, px);
        let stride = usize::from(g.width).div_ceil(4);
        let levels = (0..usize::from(g.height))
            .flat_map(|y| {
                let bits = &g.bits;
                (0..usize::from(g.width))
                    .map(move |x| (bits[y * stride + x / 4] >> (6 - 2 * (x % 4))) & 3)
            })
            .collect();
        super::Glyph {
            advance: g.advance.into(),
            left: g.offset_x.into(),
            top: -i32::from(g.offset_y),
            width: g.width.into(),
            height: g.height.into(),
            levels,
        }
    }

    pub fn kerning(data: &[u8], chars: &[char], px: f32) -> super::Kern {
        let charset: Vec<_> = chars.iter().copied().map(Some).collect();
        let units = collect_kern_units(data, &charset);
        let table = kern_classes_px(&units, px, chars.len());
        let mut result = super::Kern::new();
        for (l, &left) in chars.iter().enumerate() {
            for (r, &right) in chars.iter().enumerate() {
                let adjustment = table.lookup(l, r);
                if adjustment != 0 {
                    result.insert((left, right), i32::from(adjustment));
                }
            }
        }
        result
    }
}

const TEXT: &str = "Thin stems, open counters and quiet spacing. Zażółć gęślą jaźń. Łódź, źrebak, ćma. AVATAR To Wa fi fl 0123456789. “Really?” (Yes!); café, naïve.";
const PROBE: &str = "Hnm il1 rn m AV To fj ąćęłńóśźż";
const STYLES: [&str; 3] = ["Regular", "Bold", "Italic"];
const MODES: [&str; 4] = ["fontdue-bw", "fontdue-aa", "freetype-mono", "freetype-gray"];
type Kern = BTreeMap<(char, char), i32>;

#[derive(Clone, Debug)]
struct Glyph {
    advance: i32,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    levels: Vec<u8>,
}

// FreeType's renderer produces positive-pitch mono/gray bitmaps. reject
// other layouts before reading its buffer, rather than misreading rows.
fn unpack(data: &[u8], width: usize, height: usize, pitch: usize, mono: bool) -> Vec<u8> {
    assert!(pitch >= if mono { width.div_ceil(8) } else { width });
    assert!(data.len() >= pitch * height);
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                if mono {
                    if data[y * pitch + x / 8] & (0x80 >> (x % 8)) != 0 {
                        3
                    } else {
                        0
                    }
                } else {
                    // same thresholds as build.rs, isolating rasterization changes
                    data[y * pitch + x] / 64
                }
            })
        })
        .collect()
}

fn ft_glyph(face: &Face, ch: char, mono: bool) -> Glyph {
    let flags = LoadFlag::NO_BITMAP
        | if mono {
            LoadFlag::TARGET_MONO | LoadFlag::FORCE_AUTOHINT
        } else {
            LoadFlag::TARGET_LIGHT
        };
    assert!(
        face.get_char_index(ch as usize).is_some(),
        "missing glyph {ch:?}"
    );
    face.load_char(ch as usize, flags).expect("load glyph");
    let slot = face.glyph();
    slot.render_glyph(if mono {
        RenderMode::Mono
    } else {
        RenderMode::Normal
    })
    .expect("render glyph");
    let bitmap = slot.bitmap();
    assert!(bitmap.pitch() >= 0, "unsupported negative bitmap pitch");
    assert_eq!(
        bitmap.pixel_mode().unwrap(),
        if mono {
            PixelMode::Mono
        } else {
            PixelMode::Gray
        }
    );
    let width = bitmap.width() as u32;
    let height = bitmap.rows() as u32;
    Glyph {
        advance: ((slot.advance().x + 32) / 64) as i32,
        left: slot.bitmap_left(),
        top: slot.bitmap_top(),
        width,
        height,
        levels: unpack(
            bitmap.buffer(),
            width as usize,
            height as usize,
            bitmap.pitch() as usize,
            mono,
        ),
    }
}

fn draw(image: &mut GrayImage, g: &Glyph, x: i32, baseline: i32, bw: bool) {
    for row in 0..g.height {
        for col in 0..g.width {
            let level = g.levels[(row * g.width + col) as usize];
            let (px, py) = (x + g.left + col as i32, baseline - g.top + row as i32);
            if level != 0
                && px >= 0
                && py >= 0
                && px < image.width() as i32
                && py < image.height() as i32
            {
                let value = if bw { 0 } else { 255 - level * 85 };
                let pixel = image.get_pixel_mut(px as u32, py as u32);
                pixel[0] = pixel[0].min(value);
            }
        }
    }
}

fn adjustment(kern: &Kern, previous: Option<char>, ch: char) -> i32 {
    previous
        .filter(|p| !p.is_whitespace() && !ch.is_whitespace())
        .and_then(|p| kern.get(&(p, ch)))
        .copied()
        .unwrap_or(0)
}

fn width(text: &str, glyphs: &BTreeMap<char, Glyph>, kern: &Kern) -> i32 {
    let mut previous = None;
    text.chars()
        .map(|ch| {
            let result = adjustment(kern, previous, ch) + glyphs[&ch].advance;
            previous = Some(ch);
            result
        })
        .sum()
}

fn line(
    image: &mut GrayImage,
    text: &str,
    glyphs: &BTreeMap<char, Glyph>,
    kern: &Kern,
    baseline: i32,
    bw: bool,
) {
    let mut x = 24;
    let mut previous = None;
    for ch in text.chars() {
        x += adjustment(kern, previous, ch);
        draw(image, &glyphs[&ch], x, baseline, bw);
        x += glyphs[&ch].advance;
        previous = Some(ch);
    }
}

fn paragraph(
    image: &mut GrayImage,
    glyphs: &BTreeMap<char, Glyph>,
    kern: &Kern,
    mut y: i32,
    px: f32,
    bw: bool,
) -> i32 {
    let mut current = String::new();
    for word in TEXT.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if width(&candidate, glyphs, kern) > 432 && !current.is_empty() {
            line(image, &current, glyphs, kern, y, bw);
            y += (px * 1.3).ceil() as i32;
            current = word.to_owned();
        } else {
            current = candidate;
        }
    }
    line(image, &current, glyphs, kern, y, bw);
    y
}

fn label(image: &mut GrayImage, font: &fontdue::Font, text: &str, baseline: i32) {
    let mut x = 24;
    for ch in text.chars() {
        let glyph = production::glyph(font, ch, 14.0);
        draw(image, &glyph, x, baseline, true);
        x += glyph.advance;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .expect("usage: plump-font-compare OUTPUT_DIRECTORY");
    let output = Path::new(&output);
    fs::create_dir_all(output)?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let library = Library::init()?;
    let label_font = fontdue::Font::from_bytes(
        fs::read(production::path(&root, "Inter", "Regular"))?,
        fontdue::FontSettings::default(),
    )?;
    let mut chars: Vec<char> = TEXT.chars().chain(PROBE.chars()).collect();
    chars.sort_unstable();
    chars.dedup();
    let mut metrics = fs::File::create(output.join("metrics.csv"))?;
    writeln!(
        metrics,
        "family,px,style,mode,codepoint,advance,left,top,width,height"
    )?;

    for (family, sizes) in production::families() {
        for px in sizes {
            let mut comparison = GrayImage::from_pixel(480 * 4, 800, Luma([255]));
            for (mode_index, mode) in MODES.iter().enumerate() {
                let mut page = GrayImage::from_pixel(480, 800, Luma([255]));
                label(
                    &mut page,
                    &label_font,
                    &format!("{family} {px}px / {mode}"),
                    26,
                );
                label(
                    &mut page,
                    &label_font,
                    "same font, size, GPOS kerning; native 480 x 800",
                    48,
                );
                for (style_index, style) in STYLES.iter().enumerate() {
                    let path = production::path(&root, family, style);
                    let data = fs::read(&path)?;
                    let font = fontdue::Font::from_bytes(
                        data.as_slice(),
                        fontdue::FontSettings::default(),
                    )?;
                    let face = library.new_face(&path, 0)?;
                    face.set_pixel_sizes(0, px as u32)?;
                    let kern = production::kerning(&data, &chars, px);
                    let glyphs: BTreeMap<_, _> = chars
                        .iter()
                        .map(|&ch| {
                            let g = if mode_index < 2 {
                                production::glyph(&font, ch, px)
                            } else {
                                ft_glyph(&face, ch, mode_index == 2)
                            };
                            (ch, g)
                        })
                        .collect();
                    for (&ch, g) in &glyphs {
                        writeln!(
                            metrics,
                            "{family},{px},{style},{mode},U+{:04X},{},{},{},{},{}",
                            ch as u32, g.advance, g.left, g.top, g.width, g.height
                        )?;
                    }
                    let y = 78 + style_index as i32 * 200;
                    label(&mut page, &label_font, style, y);
                    let last_y = paragraph(&mut page, &glyphs, &kern, y + 30, px, mode_index == 0);
                    assert!(
                        last_y + px as i32 <= y + 188,
                        "sample overflows its section"
                    );
                    if style_index == 0 {
                        assert!(width(PROBE, &glyphs, &kern) <= 432, "probe too wide");
                        line(&mut page, PROBE, &glyphs, &kern, 714, mode_index == 0);
                    }
                }
                label(
                    &mut page,
                    &label_font,
                    "ideal gray tones, not a simulation of panel ink",
                    768,
                );
                let stem = format!("{family}-{px}-{mode}");
                page.save(output.join(format!("{stem}.png")))?;
                // RGB forces an uncompressed 24-bit BMP the sleep loader accepts.
                image::DynamicImage::ImageLuma8(page.clone())
                    .to_rgb8()
                    .save(output.join(format!("{stem}.bmp")))?;
                image::imageops::replace(&mut comparison, &page, mode_index as i64 * 480, 0);
            }
            comparison.save(output.join(format!("{family}-{px}-comparison.png")))?;
            let crop = image::imageops::crop_imm(&comparison, 0, 682, 1920, 42).to_image();
            image::imageops::resize(&crop, 3840, 84, image::imageops::FilterType::Nearest)
                .save(output.join(format!("{family}-{px}-strokes-2x.png")))?;
        }
    }
    println!(
        "wrote native pages, 24-bit BMPs, comparisons and metrics to {}",
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_unpack_respects_bit_order_byte_boundary_and_row_padding() {
        assert_eq!(
            unpack(&[0x81, 0x80, 0xff, 0x40, 0x00, 0xff], 9, 2, 3, true),
            [3, 0, 0, 0, 0, 0, 0, 3, 3, 0, 3, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn gray_unpack_matches_production_thresholds_and_ignores_padding() {
        assert_eq!(
            unpack(
                &[0, 63, 64, 127, 255, 128, 191, 192, 255, 0],
                4,
                2,
                5,
                false
            ),
            [0, 0, 1, 1, 2, 2, 3, 3]
        );
    }

    #[test]
    fn drawing_uses_bearings_baseline_and_bw_edge_rule() {
        let glyph = Glyph {
            advance: 3,
            left: -1,
            top: 2,
            width: 2,
            height: 2,
            levels: vec![1, 0, 3, 2],
        };
        let mut aa = GrayImage::from_pixel(4, 4, Luma([255]));
        draw(&mut aa, &glyph, 2, 3, false);
        assert_eq!(
            aa.as_raw(),
            &[
                255, 255, 255, 255, 255, 170, 255, 255, 255, 0, 85, 255, 255, 255, 255, 255
            ]
        );
        let mut bw = GrayImage::from_pixel(4, 4, Luma([255]));
        draw(&mut bw, &glyph, 2, 3, true);
        assert_eq!(bw.get_pixel(1, 1)[0], 0);
        assert_eq!(bw.get_pixel(2, 2)[0], 0);
        assert_eq!(bw.get_pixel(2, 1)[0], 255);
    }

    #[test]
    fn kerning_stops_at_spaces() {
        let kern = Kern::from([(('A', 'V'), -2), (('A', ' '), -8), ((' ', 'V'), -9)]);
        assert_eq!(adjustment(&kern, Some('A'), 'V'), -2);
        assert_eq!(adjustment(&kern, Some('A'), ' '), 0);
        assert_eq!(adjustment(&kern, Some(' '), 'V'), 0);
    }
}
