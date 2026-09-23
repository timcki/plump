// the loading sheet (mockups/xteink_x4_reader_loading_sheet.html).
//
// a load that finishes inside the hold paints nothing but the page. a
// slower one puts up the bottom sheet: over the screen the book was
// opened from when entering it (the manager draws that screen beneath,
// see `loads_over_origin`), over plain paper otherwise. the sheet is
// drawn from `SheetFace`, a snapshot taken only on steps that may
// repaint: a step inside a waveform window can move the pipeline on,
// and a sheet read live would then differ between the frame's two
// phases.

use alloc::boxed::Box;
use core::fmt::Write;

use plump_kernel::util::FixedStr;

use super::{ReaderApp, State};
use crate::apps::AppContext;
use crate::apps::widgets::LoadingSheet;
use crate::apps::widgets::row::CoverBox;
use crate::apps::widgets::sheet::{self, RowLead, RowSpec, ValueChip};
use crate::drivers::strip::{GrayMode, StripBuffer};
use crate::error::ErrorKind;
use crate::fonts;
use crate::ui::StackFmt;

use super::PAGE_REGION;

// how long a loading state may run before its sheet is painted:
// anything faster lands as one refresh, the page itself. a wake
// starts from the sleep wallpaper, which is fine to look at, and its
// bundle reopen routinely takes a second: hold longer there so the
// sheet does not flash for a moment before the page
const LOADING_HOLD_MS: u64 = 400;
const LOADING_HOLD_RESUME_MS: u64 = 1500;

// the bar moves in quarters: each step is a refresh of its own
const PCT_STEP: u32 = 25;

/// Why the reader is loading: it picks the sheet's header and what
/// lies beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoadingReason {
    Open,
    Resume,
    Chapter,
    Font,
}

/// What the user is told the load is doing: the pipeline's states
/// collapsed to the three a reader can tell apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Opening,
    Preparing,
    LayingOut,
    WaitingImages,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lead {
    /// whether the cached thumb was loaded when the face was taken
    Cover(bool),
    Icon(char),
}

/// Everything the sheet shows, as text: comparing two of these says
/// whether a repaint would change anything.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct SheetFace {
    title: FixedStr<64>,
    /// set in the book's face, not the UI's
    book_voice: bool,
    caption: FixedStr<32>,
    meta: FixedStr<64>,
    lead: Lead,
    text: FixedStr<48>,
    sub: FixedStr<64>,
    value: FixedStr<8>,
    pct: Option<u8>,
    back: &'static str,
}

impl SheetFace {
    const EMPTY: Self = Self {
        title: FixedStr::EMPTY,
        book_voice: false,
        caption: FixedStr::EMPTY,
        meta: FixedStr::EMPTY,
        lead: Lead::Icon(sheet::ICON_LIST),
        text: FixedStr::EMPTY,
        sub: FixedStr::EMPTY,
        value: FixedStr::EMPTY,
        pct: None,
        back: "",
    };
}

// copy up to N bytes, cut back to a char boundary
fn put<const N: usize>(dst: &mut FixedStr<N>, s: &str) {
    let mut n = s.len().min(N);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    dst.set(&s.as_bytes()[..n]);
}

fn put_upper<const N: usize>(dst: &mut FixedStr<N>, s: &str) {
    let mut up = StackFmt::<64>::new();
    for c in s.chars() {
        let _ = up.write_char(c.to_ascii_uppercase());
    }
    put(dst, up.as_str());
}

impl ReaderApp {
    /// The loading sheet is (or will be) drawn over the screen the
    /// book was opened from; the manager draws that screen beneath.
    /// true through the hold as well, so a frame rendered for any
    /// other reason keeps showing what the panel already shows
    pub fn loads_over_origin(&self) -> bool {
        self.loading_reason == LoadingReason::Open
            && (self.is_loading() || self.state == State::Error)
    }

    /// Start a loading episode. nothing is painted until the hold runs
    /// out or a slow step is next (`loading_tick`, `paint_loading`), so
    /// a fast open, wake or chapter change lands as one refresh
    pub(super) fn begin_loading(&mut self, reason: LoadingReason) {
        self.loading_reason = reason;
        self.loading_due = false;
        self.sheet = None;
        self.loading_since = embassy_time::Instant::now();
    }

    /// The sheet is due now; `refresh_sheet` puts it up on the next
    /// step allowed to repaint
    pub(super) fn paint_loading(&mut self) {
        if !self.loading_due {
            self.loading_due = true;
            log::info!(
                "reader: loading sheet {:?} after {} ms in {:?}",
                self.loading_reason,
                self.loading_since.elapsed().as_millis(),
                self.state
            );
        }
    }

    /// Hold check, run before every pipeline step
    pub(super) fn loading_tick(&mut self) {
        if self.loading_due || !self.is_loading() {
            return;
        }
        let hold = if self.loading_reason == LoadingReason::Resume {
            LOADING_HOLD_RESUME_MS
        } else {
            LOADING_HOLD_MS
        };
        if self.loading_since.elapsed().as_millis() >= hold {
            self.paint_loading();
        }
    }

    /// Repaint the sheet if what it shows has changed. the first
    /// paint of an episode is a full clear when this is the first
    /// frame after entering the book or waking, unless the sheet sits
    /// over the screen the book was opened from: that screen is
    /// already on the panel, and the sheet should only appear on it
    pub(super) fn refresh_sheet(&mut self, ctx: &mut AppContext, can_paint: bool) {
        if !can_paint
            || !self.loading_due
            || !(self.is_loading() || self.state == State::Error)
        {
            return;
        }
        let face = self.sheet_face();
        match self.sheet.as_deref_mut() {
            Some(shown) if *shown == face => return,
            Some(shown) => *shown = face,
            None => {
                // heap, not .bss: it only exists while a sheet is up
                self.sheet = Some(Box::new(face));
                if self.first_paint_full && !self.loads_over_origin() {
                    self.first_paint_full = false;
                    ctx.request_full_redraw();
                    return;
                }
            }
        }
        ctx.mark_dirty(PAGE_REGION);
    }

    fn loading_stage(&self) -> Stage {
        match self.state {
            State::NeedCache => Stage::Preparing,
            State::NeedIndex if self.typeset_oom_waits > 0 => Stage::WaitingImages,
            State::NeedIndex
                if self.is_epub
                    && self.epub.chapters_cached
                    && !self.epub.ch_cached[self.epub.chapter as usize] =>
            {
                Stage::Preparing
            }
            State::NeedIndex => Stage::LayingOut,
            State::NeedPage if self.is_epub => Stage::LayingOut,
            _ => Stage::Opening,
        }
    }

    /// The chapter's name from the contents, for spine index `ch`
    pub(super) fn chapter_title(&self, ch: u16) -> Option<&str> {
        let toc = self.epub.toc.as_ref()?;
        (0..toc.len())
            .map(|i| &toc.entries[i])
            .find(|e| e.spine_idx == ch && !e.title_str().is_empty())
            .map(|e| e.title_str())
    }

    /// Where the book will open: chapter and percent, "from the start"
    /// for a book without a place, page or size for plain text
    fn loading_resume_line(&self, out: &mut StackFmt<64>) {
        if self.is_epub {
            let n = self.epub.spine.len();
            let ch = self.epub.chapter as usize + 1;
            let fresh = self.epub.chapter == 0 && self.restore_offset.unwrap_or(0) == 0;
            if n == 0 {
                // before the OPF: what the bundle header knows
                match (ch > 1, self.spine_hint as usize) {
                    (true, hint) if hint > 0 => {
                        let _ = write!(out, "chapter {} of {}", ch, hint);
                    }
                    (true, _) => {
                        let _ = write!(out, "chapter {}", ch);
                    }
                    (false, hint) if hint > 0 => {
                        let _ = write!(out, "{} chapters", hint);
                    }
                    _ => {}
                }
            } else if fresh {
                let _ = write!(out, "{} chapters \u{00B7} from the start", n);
            } else {
                let _ = write!(
                    out,
                    "chapter {} of {} \u{00B7} {}% read",
                    ch,
                    n,
                    self.progress_pct()
                );
            }
        } else if self.file_size > 0 {
            match self.restore_page_hint {
                Some(p) if p > 0 => {
                    let _ = write!(out, "page {}", p + 1);
                }
                _ => {
                    let _ = write!(out, "{} KB", self.file_size / 1024);
                }
            }
        }
    }

    fn error_line(&self, kind: ErrorKind) -> &'static str {
        match kind {
            ErrorKind::NoCard => "The SD card is not responding",
            ErrorKind::OutOfMemory => "Not enough memory to open it",
            ErrorKind::ParseFailed | ErrorKind::InvalidData | ErrorKind::BadEncoding => {
                if self.is_epub {
                    "This EPUB could not be read"
                } else {
                    "This file could not be read"
                }
            }
            ErrorKind::NotFound | ErrorKind::OpenFile | ErrorKind::OpenDir => {
                "The file could not be found"
            }
            _ => "The card could not be read",
        }
    }

    fn sheet_face(&self) -> SheetFace {
        let mut f = SheetFace::EMPTY;
        let mut buf = StackFmt::<64>::new();
        let n = self.epub.spine.len();
        let ch = self.epub.chapter;

        if let Some(e) = self.error {
            put(&mut f.title, self.display_name());
            f.book_voice = self.title_is_real;
            put(&mut f.caption, "COULD NOT OPEN");
            if self.title_is_real {
                put(&mut f.meta, self.name());
            }
            f.lead = Lead::Icon(sheet::ICON_FILE);
            put(&mut f.text, self.error_line(e.kind()));
            let _ = write!(buf, "{}", e);
            put(&mut f.sub, buf.as_str());
            f.back = "CLOSE";
            return f;
        }

        match self.loading_reason {
            LoadingReason::Open | LoadingReason::Resume => {
                put(&mut f.title, self.display_name());
                f.book_voice = true;
                put(
                    &mut f.caption,
                    if self.loading_reason == LoadingReason::Open {
                        "OPENING"
                    } else {
                        "RESUMING"
                    },
                );
                let author = if self.is_epub {
                    self.epub.meta.author_str()
                } else {
                    ""
                };
                if !author.is_empty() {
                    let _ = write!(buf, "{} \u{00B7} ", author);
                }
                self.loading_resume_line(&mut buf);
                put(&mut f.meta, buf.as_str().trim_end_matches(" \u{00B7} "));
                f.lead = Lead::Cover(self.loading_cover.is_some());
                f.back = if self.loading_reason == LoadingReason::Open {
                    "CANCEL"
                } else {
                    "LEAVE BOOK"
                };
            }
            LoadingReason::Chapter => {
                match self.chapter_title(ch) {
                    Some(t) => put(&mut f.title, t),
                    None => {
                        let _ = write!(buf, "Chapter {}", ch + 1);
                        put(&mut f.title, buf.as_str());
                    }
                }
                f.book_voice = true;
                buf.clear();
                let _ = write!(buf, "CHAPTER {} OF {}", ch + 1, n);
                put(&mut f.caption, buf.as_str());
                put(&mut f.meta, self.display_name());
                f.lead = Lead::Icon(sheet::ICON_LIST);
                f.back = "LEAVE BOOK";
            }
            LoadingReason::Font => {
                put(&mut f.title, self.display_name());
                f.book_voice = true;
                let size = fonts::FONT_SIZE_NAMES
                    .get(self.book_font_size_idx as usize)
                    .copied()
                    .unwrap_or("");
                let _ = write!(buf, "{} \u{00B7} {}", self.reader_font.name(), size);
                put_upper(&mut f.caption, buf.as_str());
                if let Some(t) = self.chapter_title(ch) {
                    put(&mut f.meta, t);
                } else if self.is_epub && n > 0 {
                    buf.clear();
                    let _ = write!(buf, "chapter {} of {}", ch + 1, n);
                    put(&mut f.meta, buf.as_str());
                }
                f.lead = Lead::Icon(sheet::ICON_TEXT_AA);
                f.back = "LEAVE BOOK";
            }
        }

        buf.clear();
        match self.loading_stage() {
            Stage::Opening => put(&mut f.text, "Opening"),
            Stage::Preparing => {
                if self.loading_reason == LoadingReason::Chapter || n == 0 {
                    put(&mut f.text, "Preparing chapter");
                } else {
                    let _ = write!(buf, "Preparing chapter {} of {}", ch + 1, n);
                    put(&mut f.text, buf.as_str());
                }
                let (done, total) = self
                    .epub
                    .cache_step
                    .as_ref()
                    .map_or((0, 1), |s| s.progress());
                let pct = (done as u64 * 100 / total.max(1) as u64) as u32;
                let pct = (pct.min(100) / PCT_STEP * PCT_STEP) as u8;
                f.pct = Some(pct);
                buf.clear();
                let _ = write!(buf, "{}%", pct);
                put(&mut f.value, buf.as_str());
            }
            Stage::LayingOut => {
                put(&mut f.text, "Laying out pages");
                // the header already names the chapter on a chapter
                // change and a font change
                if matches!(self.loading_reason, LoadingReason::Open | LoadingReason::Resume)
                    && let Some(t) = self.chapter_title(ch)
                {
                    put(&mut f.sub, t);
                }
            }
            Stage::WaitingImages => put(&mut f.text, "Waiting for images"),
        }
        f
    }

    /// The sheet, as last snapshotted, over whatever lies beneath it
    pub(super) fn draw_loading_sheet(&self, strip: &mut StripBuffer) {
        let Some(f) = self.sheet.as_deref() else {
            return;
        };
        if strip.gray_mode() != GrayMode::Bw {
            return;
        }
        let title_font = if f.book_voice {
            fonts::heading_font(self.reader_font.family(), self.ui_font_idx)
        } else {
            fonts::ui_heading_font(self.ui_font_idx)
        };
        let lead = match f.lead {
            Lead::Cover(true) => RowLead::Cover(self.loading_cover.as_ref(), CoverBox::Mini),
            Lead::Cover(false) => RowLead::Cover(None, CoverBox::Mini),
            Lead::Icon(c) => RowLead::Icon(c),
        };
        LoadingSheet {
            title: f.title.as_str(),
            title_font,
            caption: f.caption.as_str(),
            meta: f.meta.as_str(),
            row: RowSpec {
                lead,
                text: f.text.as_str(),
                text_font: fonts::ui_body_font(self.ui_font_idx),
                value: f.value.as_str(),
                selected: false,
                sub: f.sub.as_str(),
                progress: f.pct.map(|p| (p as u32, 100)),
                chip: ValueChip::None,
            },
            back: f.back,
        }
        .draw(strip, self.ui_font_idx);
    }
}
