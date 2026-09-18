//! Strip one XHTML file against a set of stylesheets and summarise the
//! decoded stream: `cargo run --example strip_dump -- chapter.xhtml a.css b.css`
//! Pass `--text` to print the plain text with `[b]`/`[i]` style tags.

use smol_epub::css::CssRules;
use smol_epub::html_strip::HtmlStripStream;
use smol_epub::markup::{Align, Event, Events, SliceSource, Style};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let show_text = args.iter().any(|a| a == "--text");
    args.retain(|a| a != "--text");
    let Some(xhtml_path) = args.first() else {
        eprintln!("usage: strip_dump [--text] chapter.xhtml [style.css ...]");
        std::process::exit(2);
    };
    let xhtml = std::fs::read(xhtml_path).expect("read xhtml");

    let mut css = CssRules::new();
    for path in &args[1..] {
        let sheet = std::fs::read(path).expect("read css");
        css.parse(&sheet);
    }
    eprintln!("css rules stored: {}{}", css.len(), if css.is_full() { " (table full)" } else { "" });

    let mut stripper = HtmlStripStream::new();
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut ip = 0;
    while ip < xhtml.len() {
        let (consumed, written) = stripper.feed(&xhtml[ip..], &mut tmp, &css);
        out.extend_from_slice(&tmp[..written]);
        if consumed == 0 && written == 0 {
            ip += 1;
        } else {
            ip += consumed;
        }
    }
    let n = stripper.finish(&mut tmp);
    out.extend_from_slice(&tmp[..n]);

    let mut src = SliceSource(&out);
    let mut words = 0usize;
    let mut italic = 0usize;
    let mut bold = 0usize;
    let mut headings = 0usize;
    let mut paragraphs = 1usize;
    let mut blocks = 0usize;
    let mut indented = 0usize;
    let mut spaced = 0usize;
    let mut centered = 0usize;
    let mut images = 0usize;
    let mut unknown = 0usize;
    let mut text = String::new();
    let mut last_style = Style::PLAIN;
    for ev in Events::new(&mut src) {
        match ev {
            Event::Word { start, end, style } => {
                words += 1;
                if style.italic {
                    italic += 1;
                }
                if style.bold {
                    bold += 1;
                }
                if style.is_heading() {
                    headings += 1;
                }
                if show_text {
                    if style != last_style {
                        text.push_str(&format!(
                            "[{}{}{}{}]",
                            if style.bold { "b" } else { "" },
                            if style.italic { "i" } else { "" },
                            if style.underline { "u" } else { "" },
                            if style.heading != 0 { format!("h{}", style.heading) } else { String::new() }
                        ));
                        last_style = style;
                    }
                    text.push_str(&String::from_utf8_lossy(&out[start as usize..end as usize]));
                }
            }
            Event::Space { .. } | Event::Nbsp { .. } => {
                if show_text {
                    text.push(' ');
                }
            }
            Event::SoftHyphen { .. } => {}
            Event::HardBreak { .. } => {
                if show_text {
                    text.push('\n');
                }
            }
            Event::ParagraphBreak { .. } => {
                paragraphs += 1;
                if show_text {
                    text.push_str("\n\n");
                }
            }
            Event::Block { block, .. } => {
                blocks += 1;
                if block.text_indent_qem > 0 {
                    indented += 1;
                }
                if block.space_above_qem > 0 {
                    spaced += 1;
                }
                if block.align == Align::Center {
                    centered += 1;
                }
                if show_text {
                    text.push_str(&format!(
                        "{{{}{}{}{}}}",
                        match block.align {
                            Align::Default => "",
                            Align::Left => "L",
                            Align::Center => "C",
                            Align::Right => "R",
                        },
                        if block.left > 0 { format!(">{}", block.left) } else { String::new() },
                        if block.text_indent_qem > 0 { format!(" ti{}", block.text_indent_qem) } else { String::new() },
                        if block.space_above_qem > 0 { format!(" sp{}", block.space_above_qem) } else { String::new() },
                    ));
                }
            }
            Event::Image(img) => {
                images += 1;
                if show_text {
                    text.push_str(&format!("[img {}]", String::from_utf8_lossy(img.path(&out))));
                }
            }
            Event::PageBreak { .. } => {
                if show_text {
                    text.push_str("[page]\n");
                }
            }
            Event::ThematicBreak { .. } => {
                if show_text {
                    text.push_str("[hr]\n");
                }
            }
            Event::Unknown { .. } => unknown += 1,
        }
    }
    eprintln!(
        "{} bytes in, {} bytes out; words {} (italic {}, bold {}, heading {}); paragraphs {}; block records {} (indented {}, spaced {}, centered {}); images {}; unknown markers {}",
        xhtml.len(), out.len(), words, italic, bold, headings, paragraphs, blocks, indented, spaced, centered, images, unknown
    );
    if show_text {
        println!("{text}");
    }
}
