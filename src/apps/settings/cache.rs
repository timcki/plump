// per-book cache administration: the sheet behind Settings > Book Cache
// (mockups/xteink_x4_settings_book_cache.html).
//
// one book leaves three kinds of bytes on the card:
//
//   _PLUMP/BOOKS/<H8>.BIN      bundle: spine, content, page index, covers
//   _PLUMP/_HHHHHHH/<h8>.BIN   one dithered inline image each
//   the bookmark slot, _PLUMP/STATS/<filename>, the RECENT record
//
// the first two are derived from the EPUB and rebuild themselves on the
// next open; the third is the only per-book state on the card that
// nothing can reconstruct. hence two scopes: Rebuild drops the derived
// bytes and fires on one press, Forget adds the irreplaceable ones and
// takes a second.
//
// both hashes come from the case-sensitive `fnv1a` of the filename, the
// same `name_hash` the reader keys its bundle and image directory with.
// stats are keyed by the filename itself, so they outlive a rebuild;
// bookmarks hash their own key, case-folded, inside the cache.

use core::fmt::Write as _;

use plump_kernel::kernel::bundle;
use plump_kernel::util::{FixedStr, hash};
use smol_epub::cache;

use crate::apps::widgets::list::ListSelection;
use crate::apps::widgets::sheet::{self, HintSlot, RowLead, RowSpec, SheetFonts, SheetGeom, ValueChip};
use crate::apps::{AppContext, RECENT_FILE, recent::RecentRecord};
use crate::drivers::strip::StripBuffer;
use crate::kernel::KernelHandle;
use crate::ui::{Region, StackFmt};

/// books the sheet will hold. the bookmark LRU keeps 16, so a card
/// with more cached books than this is already past the point where
/// the device remembers where you were in them.
pub const MAX_BOOKS: usize = 20;

/// filenames come from `DirEntry::name`, which is a FAT short name:
/// 8.3 plus the dot, so 12 bytes is the most that can arrive.
const NAME_CAP: usize = 16;

/// the picker row ellipsizes past roughly 35 characters at body size,
/// so a longer title would only be cut on screen anyway. 20 entries
/// of this table sit in .bss for the life of the device
const TITLE_CAP: usize = 40;

const STATS_DIR: &str = crate::apps::stats::STATS_DIR;

// ── what a clear takes with it ──────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// bundle + images; keeps the reading position and the stats
    Rebuild,
    /// everything, position and reading time included
    Forget,
}

impl Scope {
    const fn label(self) -> &'static str {
        match self {
            Self::Rebuild => "Rebuild cache",
            Self::Forget => "Forget book",
        }
    }

    const fn sub(self) -> &'static str {
        match self {
            Self::Rebuild => "keeps your place and reading time",
            Self::Forget => "cache, place and reading time",
        }
    }

    const fn icon(self) -> char {
        match self {
            Self::Rebuild => sheet::ICON_ARROWS_CLOCKWISE,
            Self::Forget => sheet::ICON_TRASH,
        }
    }

    const fn done_caption(self) -> &'static str {
        match self {
            Self::Rebuild => "CLEARED",
            Self::Forget => "FORGOTTEN",
        }
    }
}

// ── one book's footprint ────────────────────────────────────────────

#[derive(Clone, Copy)]
struct BookEntry {
    filename: FixedStr<NAME_CAP>,
    title: FixedStr<TITLE_CAP>,
    name_hash: u32,
    bundle: u32,
    images: u32,
    image_files: u16,
    /// sized already; until then the row reads "measuring"
    measured: bool,
    /// the book the continue-reading card names
    recent: bool,
}

impl BookEntry {
    const EMPTY: Self = Self {
        filename: FixedStr::EMPTY,
        title: FixedStr::EMPTY,
        name_hash: 0,
        bundle: 0,
        images: 0,
        image_files: 0,
        measured: false,
        recent: false,
    };

    #[inline]
    fn total(&self) -> u32 {
        self.bundle.saturating_add(self.images)
    }

    fn display(&self) -> &str {
        if self.title.is_empty() {
            self.filename.as_str()
        } else {
            self.title.as_str()
        }
    }

    /// per-book image cache directory, `_PLUMP/_HHHHHHH/`
    fn image_dir(&self) -> [u8; 8] {
        cache::dir_name_for_hash(self.name_hash)
    }
}

// ── the clear in flight ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClearStep {
    Bundle,
    Images,
    /// the directory entry itself, once its files are gone
    Dir,
    /// bookmark, stats and RECENT: Forget only
    Extras,
    Finished,
}

#[derive(Clone, Copy)]
struct Clearing {
    scope: Scope,
    step: ClearStep,
    filename: FixedStr<NAME_CAP>,
    title: FixedStr<TITLE_CAP>,
    name_hash: u32,
    bundle: u32,
    bundle_freed: bool,
    images_total: u16,
    images_left: u16,
    freed: u32,
    /// last painted tenth of the image pass; each repaint is a DU, so
    /// the stage row only follows the decades
    painted_tenth: u8,
}

impl Clearing {
    fn for_entry(entry: &BookEntry, scope: Scope) -> Self {
        Self {
            scope,
            step: ClearStep::Bundle,
            filename: entry.filename,
            title: entry.title,
            name_hash: entry.name_hash,
            bundle: entry.bundle,
            bundle_freed: false,
            images_total: entry.image_files,
            images_left: entry.image_files,
            freed: 0,
            painted_tenth: u8::MAX,
        }
    }

    fn images_done(&self) -> u16 {
        self.images_total.saturating_sub(self.images_left)
    }

    fn tenth(&self) -> u8 {
        if self.images_total == 0 {
            return 10;
        }
        (self.images_done() as u32 * 10 / self.images_total as u32) as u8
    }
}

// ── stages ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// every book with a cache, largest first
    Picker,
    /// one book: the two scopes over the breakdown
    Book,
    /// Forget, one press from firing
    Armed,
    Clearing,
    Done,
}

/// SD work the sheet owes. The input path has no `KernelHandle`, so a
/// press only records what it needs and the background budget pays
/// for it, which is also what keeps a press from waiting on the card.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// re-list the books and size them
    Enumerate,
    /// place and reading time of the focused book
    Position,
}

/// What the sheet did with a press the settings screen handed it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SheetResult {
    /// handled; the sheet stays open
    Consumed,
    /// handled; the sheet closed
    Closed,
}

pub struct CacheSheet {
    open: bool,
    stage: Stage,
    entries: [BookEntry; MAX_BOOKS],
    count: usize,
    sel: ListSelection,
    /// which scope row the Book stage has focused
    action: usize,
    /// the book the Book / Armed stages act on
    target: usize,
    clearing: Option<Clearing>,
    /// what the last clear freed, and the scope it ran, for Done
    done: Option<(Scope, FixedStr<TITLE_CAP>, u32)>,
    /// place and reading time of the focused book, read when the Book
    /// stage opens so the breakdown can price what Forget takes
    target_chapter: Option<u16>,
    target_secs: u32,
    /// entries still to size
    scanning: bool,
    pending: Pending,
    /// the total behind the Book Cache row changed. set when a scan
    /// completes and when a clear lands, not per measured book: each
    /// repaint of that row is a DU, so it follows the answer rather
    /// than the progress
    row_dirty: bool,
    geom: SheetGeom,
    ui_font_idx: u8,
}

impl Default for CacheSheet {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheSheet {
    pub const fn new() -> Self {
        Self {
            open: false,
            stage: Stage::Picker,
            entries: [BookEntry::EMPTY; MAX_BOOKS],
            count: 0,
            sel: ListSelection::new(0, 0),
            action: 0,
            target: 0,
            clearing: None,
            done: None,
            target_chapter: None,
            target_secs: 0,
            scanning: false,
            pending: Pending::None,
            row_dirty: false,
            geom: SheetGeom::full(None, sheet::ROW_H),
            ui_font_idx: 1,
        }
    }

    #[inline]
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_font_idx = idx;
        // rows are sized from the UI face, so the sheet and the
        // scroll window have to be re-derived with it
        self.resync_geom();
    }

    // ── the settings row's own value ────────────────────────────────

    /// `9 books · 4.5 MB` for the Book Cache row, or the scan in
    /// progress. Written before the sheet has ever been opened, so it
    /// reads from whatever the last enumeration left behind.
    pub fn summary(&self, out: &mut impl core::fmt::Write) {
        if self.count == 0 {
            let _ = out.write_str(if self.scanning { "measuring" } else { "empty" });
            return;
        }
        let mut bytes = 0u32;
        for e in self.entries[..self.count].iter() {
            bytes = bytes.saturating_add(e.total());
        }
        let _ = write!(out, "{} book", self.count);
        if self.count != 1 {
            let _ = out.write_char('s');
        }
        let _ = out.write_str(" \u{00B7} ");
        write_bytes(out, bytes);
    }

    /// The Book Cache row's value cell: `4.5 MB` and the arrow that
    /// marks the one settings row which opens a surface instead of
    /// stepping a value. The count rides on the sheet's own header,
    /// which has the width for it.
    pub fn row_value(&self, out: &mut impl core::fmt::Write) {
        self.row_value_plain(out);
        let _ = out.write_str(" \u{2192}");
    }

    /// The same figure without the arrow, for a row that does not
    /// open anything (the About group's own line).
    pub fn row_value_plain(&self, out: &mut impl core::fmt::Write) {
        if self.scanning && self.count == 0 {
            let _ = out.write_str("measuring");
        } else if self.count == 0 {
            let _ = out.write_str("empty");
        } else {
            let mut bytes = 0u32;
            for e in self.entries[..self.count].iter() {
                bytes = bytes.saturating_add(e.total());
            }
            write_bytes(out, bytes);
        }
    }

    // ── opening and closing ─────────────────────────────────────────

    /// Ask for a fresh listing. Called when the settings tab opens, so
    /// the Book Cache row can show the total before anyone opens the
    /// sheet, and the sheet opens on a list that is already sized.
    pub fn request_scan(&mut self) {
        if self.clearing.is_some() {
            return;
        }
        self.pending = Pending::Enumerate;
    }

    /// Open on the picker. No I/O: whatever the scan has reached is
    /// what shows, and unsized rows read "measuring".
    pub fn open(&mut self) {
        self.stage = Stage::Picker;
        self.clearing = None;
        self.done = None;
        self.open = true;
        self.resync_geom();
    }

    pub fn close(&mut self) {
        self.open = false;
        if let Some(job) = self.clearing.take() {
            // only reachable through a long Back, which leaves the tab
            // outright. the leftover is a cold open, nothing worse
            log::info!(
                "cache: clear of '{}' abandoned after {} B",
                job.filename.as_str(),
                job.freed
            );
        }
    }

    fn enumerate(&mut self, k: &mut KernelHandle<'_>) {
        self.entries = [BookEntry::EMPTY; MAX_BOOKS];
        self.count = 0;

        if k.ensure_dir_cache_loaded().is_err() {
            self.scanning = false;
            self.sel = ListSelection::new(0, 0);
            return;
        }

        let recent = read_recent_filename(k);

        let mut page = [crate::drivers::storage::DirEntry::EMPTY; MAX_BOOKS];
        let mut offset = 0usize;
        while let Ok(res) = k.dir_page(offset, &mut page) {
            if res.count == 0 {
                break;
            }
            for entry in page.iter().take(res.count) {
                if self.count >= MAX_BOOKS {
                    break;
                }
                if entry.is_dir {
                    continue;
                }
                let name = entry.name_str();
                self.entries[self.count] = BookEntry {
                    filename: FixedStr::from_bytes(name.as_bytes()),
                    title: FixedStr::from_bytes(entry.display_name().as_bytes()),
                    name_hash: hash::fnv1a(name.as_bytes()),
                    recent: recent.eq_ignore_ascii_case(name.as_bytes()),
                    ..BookEntry::EMPTY
                };
                self.count += 1;
            }
            offset += res.count;
            if offset >= res.total || self.count >= MAX_BOOKS {
                break;
            }
        }

        self.scanning = self.count > 0;
        self.sel = ListSelection::new(self.count, 0);
        // the picker is sized to the list, so a fresh count resizes it
        self.resync_geom();
        log::info!("cache: {} candidate books to size", self.count);
    }

    // ── sizing, one book per background step ────────────────────────

    /// Size one unmeasured book. Two directory walks: the bundle's
    /// entry in `BOOKS/`, and the book's image directory. A book with
    /// neither drops out of the list.
    fn measure_step(&mut self, k: &mut KernelHandle<'_>) -> bool {
        if !self.scanning {
            return false;
        }
        let Some(idx) = self.entries[..self.count].iter().position(|e| !e.measured) else {
            self.finish_scan();
            return false;
        };

        let entry = &mut self.entries[idx];
        entry.bundle = bundle::file_size(k.sd(), entry.name_hash).unwrap_or(0);
        let dir_buf = entry.image_dir();
        match k.sd().measure_plump_subdir(cache::dir_name_str(&dir_buf)) {
            Ok(usage) => {
                entry.images = usage.bytes;
                entry.image_files = usage.files;
            }
            // no image directory: the common case for a book without
            // figures, and indistinguishable from one here
            Err(_) => {
                entry.images = 0;
                entry.image_files = 0;
            }
        }
        entry.measured = true;

        if entry.total() == 0 {
            self.remove(idx);
        }
        if self.entries[..self.count].iter().all(|e| e.measured) {
            self.finish_scan();
        }
        true
    }

    /// Largest first, once every row has a size to sort by.
    fn finish_scan(&mut self) {
        self.scanning = false;
        self.row_dirty = true;
        // insertion sort: 20 entries at most, and only once per open
        for i in 1..self.count {
            let key = self.entries[i];
            let mut j = i;
            while j > 0 && self.entries[j - 1].total() < key.total() {
                self.entries[j] = self.entries[j - 1];
                j -= 1;
            }
            self.entries[j] = key;
        }
        self.sel.set_count(self.count);
        self.sel.selected = 0;
        self.sel.scroll = 0;
    }

    fn remove(&mut self, idx: usize) {
        for i in idx..self.count.saturating_sub(1) {
            self.entries[i] = self.entries[i + 1];
        }
        self.count = self.count.saturating_sub(1);
        self.entries[self.count] = BookEntry::EMPTY;
        if self.sel.selected > idx {
            self.sel.selected -= 1;
        }
        self.sel.set_count(self.count);
    }

    // ── clearing, bounded per background step ───────────────────────

    /// Advance the clear one step. Bounded so a book with a hundred
    /// cached figures clears across several steps instead of holding
    /// the event loop: each unlink walks the directory.
    ///
    /// Returns true when the caller should repaint.
    fn clear_step(&mut self, k: &mut KernelHandle<'_>) -> bool {
        let Some(mut job) = self.clearing else {
            return false;
        };

        let mut repaint = false;
        match job.step {
            ClearStep::Bundle => {
                match bundle::delete(k.sd(), job.name_hash) {
                    Ok(()) => {
                        job.freed = job.freed.saturating_add(job.bundle);
                        job.bundle_freed = true;
                    }
                    Err(e) => log::warn!("cache: bundle delete failed: {}", e),
                }
                job.step = ClearStep::Images;
                repaint = true;
            }

            ClearStep::Images => {
                let dir_buf = cache::dir_name_for_hash(job.name_hash);
                let dir = cache::dir_name_str(&dir_buf);
                match k.sd().purge_plump_subdir(dir) {
                    Ok(step) => {
                        job.freed = job.freed.saturating_add(step.bytes);
                        job.images_left = job.images_left.saturating_sub(step.deleted);
                        // a batch that unlinked nothing would walk the
                        // same entries again forever, so it ends the
                        // pass whatever it reported
                        if !step.more || step.deleted == 0 {
                            job.step = ClearStep::Dir;
                        }
                    }
                    Err(_) => job.step = ClearStep::Dir,
                }
                let tenth = job.tenth();
                if tenth != job.painted_tenth {
                    job.painted_tenth = tenth;
                    repaint = true;
                }
            }

            ClearStep::Dir => {
                let dir_buf = cache::dir_name_for_hash(job.name_hash);
                let dir = cache::dir_name_str(&dir_buf);
                // a book with no figures never had one
                if let Err(e) = k.sd().remove_plump_subdir(dir) {
                    log::debug!("cache: no image dir {} to remove ({})", dir, e);
                }
                job.step = ClearStep::Extras;
            }

            ClearStep::Extras => {
                if job.scope == Scope::Forget {
                    self.forget_extras(k, &job);
                }
                job.step = ClearStep::Finished;
            }

            ClearStep::Finished => {}
        }

        if job.step == ClearStep::Finished {
            log::info!(
                "cache: {} '{}' freed {} B",
                match job.scope {
                    Scope::Rebuild => "rebuilt",
                    Scope::Forget => "forgot",
                },
                job.filename.as_str(),
                job.freed
            );
            self.done = Some((job.scope, job.title, job.freed));
            self.settle_entry(&job);
            self.clearing = None;
            self.stage = Stage::Done;
            self.resync_geom();
            return true;
        }

        self.clearing = Some(job);
        repaint
    }

    /// The position and reading time, which nothing rebuilds.
    fn forget_extras(&mut self, k: &mut KernelHandle<'_>, job: &Clearing) {
        let name = job.filename;
        if k.bookmarks().forget(name.as_bytes()) {
            log::info!("cache: dropped bookmark for '{}'", name.as_str());
        }
        if let Err(e) = k.sd().delete_in_plump_subdir(STATS_DIR, name.as_str()) {
            log::debug!("cache: no stats file for '{}' ({})", name.as_str(), e);
        }
        // the continue-reading card reads RECENT directly; the recent
        // rows come from the bookmark LRU and are already handled
        if read_recent_filename(k).eq_ignore_ascii_case(name.as_bytes()) {
            let mut buf = [0u8; crate::apps::recent::BUF_LEN];
            let n = RecentRecord::EMPTY.encode(&mut buf);
            if let Err(e) = k
                .sd()
                .write_file_in_dir(k.sd().data_dir(), RECENT_FILE, &buf[..n])
            {
                log::warn!("cache: clearing RECENT failed: {}", e);
            }
        }
    }

    /// Fold the result back into the list.
    ///
    /// The sizes go to zero so every total on screen is right at once,
    /// and the entry is queued for a re-measure to confirm it: a clear
    /// the user stopped, or an unlink that failed, then shows its real
    /// leftover instead of vanishing. A book that really is at zero
    /// leaves the list on that re-measure, since the list only holds
    /// books with a cache.
    fn settle_entry(&mut self, job: &Clearing) {
        let Some(idx) = self.entries[..self.count]
            .iter()
            .position(|e| e.name_hash == job.name_hash)
        else {
            return;
        };
        let entry = &mut self.entries[idx];
        entry.bundle = 0;
        entry.images = 0;
        entry.image_files = 0;
        entry.measured = false;
        self.scanning = true;
        self.row_dirty = true;
    }

    // ── input ───────────────────────────────────────────────────────

    /// Up / Down.
    pub fn on_vertical(&mut self, down: bool, ctx: &mut AppContext) -> SheetResult {
        match self.stage {
            Stage::Picker => {
                let before = self.sel.scroll;
                let moved = if down {
                    self.sel.move_next()
                } else {
                    self.sel.move_prev()
                };
                if moved {
                    if self.sel.scroll == before {
                        ctx.mark_dirty(self.rows_region());
                    } else {
                        ctx.mark_dirty(self.geom.region);
                    }
                }
            }
            Stage::Book => {
                let next = if down { 1 } else { 0 };
                if next != self.action {
                    self.action = next;
                    ctx.mark_dirty(self.rows_region());
                }
            }
            // one row, or none to move over
            Stage::Armed | Stage::Clearing | Stage::Done => {}
        }
        SheetResult::Consumed
    }

    /// Select.
    pub fn on_select(&mut self, ctx: &mut AppContext) -> SheetResult {
        match self.stage {
            Stage::Picker => {
                if self.count == 0 {
                    self.close();
                    ctx.request_full_redraw();
                    return SheetResult::Closed;
                }
                self.target = self.sel.selected;
                self.action = 0;
                self.pending = Pending::Position;
                self.goto(Stage::Book, ctx);
            }
            Stage::Book => {
                let scope = self.focused_scope();
                match scope {
                    // nothing Rebuild deletes is unrecoverable, so it
                    // fires on the first press
                    Scope::Rebuild => self.begin_clear(Scope::Rebuild, ctx),
                    Scope::Forget => self.goto(Stage::Armed, ctx),
                }
            }
            Stage::Armed => self.begin_clear(Scope::Forget, ctx),
            Stage::Clearing => {}
            Stage::Done => self.goto(Stage::Picker, ctx),
        }
        SheetResult::Consumed
    }

    /// Back, and Menu, which closes the sheet outright.
    pub fn on_back(&mut self, ctx: &mut AppContext) -> SheetResult {
        match self.stage {
            Stage::Picker => {
                self.close();
                ctx.request_full_redraw();
                SheetResult::Closed
            }
            Stage::Book => {
                self.goto(Stage::Picker, ctx);
                SheetResult::Consumed
            }
            Stage::Armed => {
                self.goto(Stage::Book, ctx);
                SheetResult::Consumed
            }
            // stopping leaves a half-cleared cache, which is what a
            // cold open already copes with
            Stage::Clearing => {
                if let Some(job) = self.clearing.take() {
                    log::info!(
                        "cache: clear of '{}' stopped, {} B freed",
                        job.filename.as_str(),
                        job.freed
                    );
                    self.done = Some((job.scope, job.title, job.freed));
                    self.settle_entry(&job);
                }
                self.goto(Stage::Done, ctx);
                SheetResult::Consumed
            }
            Stage::Done => {
                self.close();
                ctx.request_full_redraw();
                SheetResult::Closed
            }
        }
    }

    pub fn on_menu(&mut self, ctx: &mut AppContext) -> SheetResult {
        // a clear is short and already has Back to stop it; closing
        // the sheet from under it would abandon it mid-directory
        if self.stage == Stage::Clearing {
            return SheetResult::Consumed;
        }
        self.close();
        ctx.request_full_redraw();
        SheetResult::Closed
    }

    /// Move to another stage. A stage change swaps all but the sheet
    /// frame and resizes it, so it repaints on a full clear rather
    /// than a DU over most of the screen: the delta a DU would drive
    /// is the whole sheet, and what it leaves behind is a ghost of
    /// the screen it replaced showing through the paper.
    fn goto(&mut self, stage: Stage, ctx: &mut AppContext) {
        self.stage = stage;
        self.resync_geom();
        ctx.request_full_redraw();
    }

    fn begin_clear(&mut self, scope: Scope, ctx: &mut AppContext) {
        let Some(entry) = self.entries[..self.count].get(self.target) else {
            return;
        };
        self.clearing = Some(Clearing::for_entry(entry, scope));
        self.goto(Stage::Clearing, ctx);
    }

    fn focused_scope(&self) -> Scope {
        if self.action == 0 {
            Scope::Rebuild
        } else {
            Scope::Forget
        }
    }

    /// Place and reading time of the focused book: one RAM lookup and
    /// one small file read, so the breakdown can price what Forget
    /// takes rather than describing it.
    fn load_target_position(&mut self, k: &mut KernelHandle<'_>) {
        let Some(entry) = self.entries[..self.count].get(self.target) else {
            return;
        };
        let name = entry.filename;
        self.target_chapter = k
            .bookmarks()
            .find(name.as_bytes())
            .map(|slot| slot.chapter);
        self.target_secs = crate::apps::stats::ReadingStats::load(k, name.as_str())
            .map(|s| s.time_secs)
            .unwrap_or(0);
    }

    /// Whether the Book Cache row's value needs repainting.
    #[inline]
    pub fn take_row_dirty(&mut self) -> bool {
        core::mem::take(&mut self.row_dirty)
    }

    /// Work left for `background_step`.
    #[inline]
    pub fn has_work(&self) -> bool {
        self.pending != Pending::None || self.scanning || self.clearing.is_some()
    }

    /// One step of whatever the sheet owes the card. Returns true when
    /// it did something, so the caller can report progress; anything
    /// that changed the screen has marked itself dirty.
    pub fn background_step(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) -> bool {
        match core::mem::replace(&mut self.pending, Pending::None) {
            Pending::Enumerate => {
                self.enumerate(k);
                if self.open {
                    ctx.mark_dirty(self.geom.region);
                }
                return true;
            }
            Pending::Position => {
                self.load_target_position(k);
                if self.open && self.stage == Stage::Book {
                    ctx.mark_dirty(self.rows_region());
                }
                return true;
            }
            Pending::None => {}
        }

        // a clear in flight outranks sizing: it is the thing the user
        // is watching
        if self.clearing.is_some() {
            if self.clear_step(k) {
                ctx.mark_dirty(self.geom.region);
            }
            return true;
        }

        if self.measure_step(k) {
            if self.open && self.stage == Stage::Picker {
                ctx.mark_dirty(self.geom.region);
            }
            return true;
        }
        false
    }

    // ── geometry ────────────────────────────────────────────────────

    fn resync_geom(&mut self) {
        let fonts = SheetFonts::for_ui(self.ui_font_idx);
        // a listing row names a book over a line of context, which is
        // two lines of text and needs the height for both; the
        // breakdown under the scopes is one line of chrome text per
        // figure, and a short row is the right size for it
        let listing = fonts.row_h(fonts.body, true, false);
        let plain = fonts.row_h(fonts.small, false, false);

        // the picker hugs its list: three cached books get a
        // three-row sheet rather than a screen-tall outline drawn
        // around them. its window is re-derived whatever stage is on
        // screen, because a clear that drops a book renumbers it
        let picker = {
            let full = SheetGeom::full(None, listing);
            if self.count >= full.rows {
                full
            } else {
                SheetGeom::anchored(self.count, None, listing)
            }
        };
        self.sel.set_visible(picker.rows);

        self.geom = match self.stage {
            Stage::Picker => picker,
            // two scopes, then the breakdown they act on
            Stage::Book => SheetGeom::anchored_groups(5, Some(1), [listing, plain]),
            Stage::Armed => SheetGeom::anchored(1, None, listing),
            Stage::Done => SheetGeom::anchored(1, None, fonts.row_h(fonts.body, false, false)),
            Stage::Clearing => SheetGeom::anchored(2, None, fonts.row_h(fonts.body, false, true)),
        };
    }

    /// The rows band alone, for a cursor move that did not scroll.
    fn rows_region(&self) -> Region {
        let rows = self.visible_rows();
        if rows == 0 {
            return self.geom.region;
        }
        let first = self.geom.row_region(0);
        let last = self.geom.row_region(rows - 1);
        Region::new(
            first.x,
            first.y,
            first.w,
            last.y + last.h + 1 - first.y,
        )
    }

    fn visible_rows(&self) -> usize {
        match self.stage {
            Stage::Picker => self.sel.visible_count().min(self.geom.rows),
            Stage::Book => 5,
            Stage::Clearing => 2,
            Stage::Armed | Stage::Done => 1,
        }
    }

    // ── drawing ─────────────────────────────────────────────────────

    pub fn draw(&self, strip: &mut StripBuffer) {
        if !self.open {
            return;
        }
        let fonts = SheetFonts::for_ui(self.ui_font_idx);
        sheet::draw_frame(strip, &self.geom);
        match self.stage {
            Stage::Picker => self.draw_picker(strip, &fonts),
            Stage::Book => self.draw_book(strip, &fonts),
            Stage::Armed => self.draw_armed(strip, &fonts),
            Stage::Clearing => self.draw_clearing(strip, &fonts),
            Stage::Done => self.draw_done(strip, &fonts),
        }
    }

    fn draw_picker(&self, strip: &mut StripBuffer, fonts: &SheetFonts) {
        let mut meta = StackFmt::<64>::new();
        if self.count == 0 {
            let _ = meta.write_str(if self.scanning {
                "Measuring what is cached\u{2026}"
            } else {
                "No cached books"
            });
        } else {
            self.summary(&mut meta);
            let _ = meta.write_str(" cached");
        }
        let caption = if self.scanning {
            "MEASURING"
        } else {
            "LARGEST FIRST"
        };
        sheet::draw_header(
            strip,
            &self.geom,
            fonts,
            "Book Cache",
            caption,
            meta.as_str(),
        );
        sheet::draw_groups(strip, &self.geom);

        let rows = self.visible_rows();
        let mut value = StackFmt::<20>::new();
        for i in 0..rows {
            let idx = self.sel.scroll + i;
            let Some(entry) = self.entries[..self.count].get(idx) else {
                break;
            };
            value.clear();
            if entry.measured {
                write_bytes(&mut value, entry.total());
            } else {
                let _ = value.write_str("measuring");
            }
            let mut sub = StackFmt::<48>::new();
            if entry.image_files > 0 {
                let _ = write!(sub, "{} images", entry.image_files);
            }
            if entry.recent {
                if !sub.as_str().is_empty() {
                    let _ = sub.write_str(" \u{00B7} ");
                }
                let _ = sub.write_str("reading now");
            }
            sheet::draw_row(
                strip,
                &self.geom,
                i,
                fonts,
                &RowSpec {
                    lead: if entry.recent {
                        RowLead::Bookmark
                    } else {
                        RowLead::Number(idx as u16 + 1)
                    },
                    text: entry.display(),
                    text_font: fonts.body,
                    value: value.as_str(),
                    selected: idx == self.sel.selected,
                    sub: sub.as_str(),
                    progress: None,
                    chip: ValueChip::None,
                },
            );
        }

        let hints: &[(HintSlot, &str)] = if self.count == 0 {
            &[(HintSlot::Back, "CLOSE")]
        } else {
            &[(HintSlot::Back, "CLOSE"), (HintSlot::Ok, "OPEN")]
        };
        sheet::draw_hints(strip, &self.geom, fonts, hints);
    }

    fn draw_book(&self, strip: &mut StripBuffer, fonts: &SheetFonts) {
        let Some(entry) = self.entries[..self.count].get(self.target) else {
            return;
        };

        let mut meta = StackFmt::<64>::new();
        write_bytes(&mut meta, entry.total());
        if entry.image_files > 0 {
            let _ = write!(meta, " \u{00B7} {} images", entry.image_files);
        }
        if entry.recent {
            let _ = meta.write_str(" \u{00B7} reading now");
        }
        sheet::draw_header(
            strip,
            &self.geom,
            fonts,
            entry.display(),
            "CACHE",
            meta.as_str(),
        );
        sheet::draw_groups(strip, &self.geom);

        // the two scopes
        let mut value = StackFmt::<20>::new();
        for (i, scope) in [Scope::Rebuild, Scope::Forget].into_iter().enumerate() {
            value.clear();
            write_bytes(&mut value, entry.total());
            sheet::draw_row(
                strip,
                &self.geom,
                i,
                fonts,
                &RowSpec {
                    lead: RowLead::Icon(scope.icon()),
                    text: scope.label(),
                    text_font: fonts.body,
                    value: value.as_str(),
                    selected: i == self.action,
                    sub: scope.sub(),
                    progress: None,
                    chip: ValueChip::None,
                },
            );
        }

        // the receipt for those numbers. not selectable: it is what
        // the rows above act on, and its last line is the only thing
        // Forget takes that Rebuild does not
        let mut bundle_v = StackFmt::<20>::new();
        write_bytes(&mut bundle_v, entry.bundle);
        let mut images_v = StackFmt::<20>::new();
        write_bytes(&mut images_v, entry.images);
        let mut place_v = StackFmt::<20>::new();
        match (self.target_chapter, self.target_secs) {
            (None, 0) => {
                let _ = place_v.write_str("none");
            }
            (chapter, secs) => {
                if let Some(ch) = chapter {
                    let _ = write!(place_v, "ch {}", ch + 1);
                }
                if secs > 0 {
                    if !place_v.as_str().is_empty() {
                        let _ = place_v.write_str(" \u{00B7} ");
                    }
                    write_duration(&mut place_v, secs);
                }
            }
        }

        let breakdown: [(&str, &str); 3] = [
            ("Text and layout", bundle_v.as_str()),
            ("Images", images_v.as_str()),
            ("Place and reading time", place_v.as_str()),
        ];
        for (i, (text, value)) in breakdown.into_iter().enumerate() {
            sheet::draw_row(
                strip,
                &self.geom,
                2 + i,
                fonts,
                &RowSpec {
                    lead: RowLead::None,
                    text,
                    text_font: fonts.small,
                    value,
                    selected: false,
                    sub: "",
                    progress: None,
                    chip: ValueChip::None,
                },
            );
        }

        sheet::draw_hints(strip, &self.geom, fonts, &[(HintSlot::Back, "LIST"), (HintSlot::Ok, "SELECT")]);
    }

    fn draw_armed(&self, strip: &mut StripBuffer, fonts: &SheetFonts) {
        let Some(entry) = self.entries[..self.count].get(self.target) else {
            return;
        };

        // what goes, itemized, in place of a sentence the chrome font
        // would have to ellipsize
        let mut meta = StackFmt::<64>::new();
        write_bytes(&mut meta, entry.total());
        let _ = meta.write_str(" \u{00B7} your place");
        if self.target_secs > 0 {
            let _ = meta.write_str(" \u{00B7} ");
            write_duration(&mut meta, self.target_secs);
            let _ = meta.write_str(" read");
        }
        sheet::draw_header(
            strip,
            &self.geom,
            fonts,
            entry.display(),
            "FORGET BOOK",
            meta.as_str(),
        );
        sheet::draw_groups(strip, &self.geom);
        sheet::draw_row(
            strip,
            &self.geom,
            0,
            fonts,
            &RowSpec {
                lead: RowLead::Icon(sheet::ICON_TRASH),
                text: "Delete all of it",
                text_font: fonts.body,
                value: "",
                selected: true,
                sub: "the book file itself stays on the card",
                progress: None,
                chip: ValueChip::None,
            },
        );
        sheet::draw_hints(strip, &self.geom, fonts, &[(HintSlot::Back, "CANCEL"), (HintSlot::Ok, "CONFIRM")]);
    }

    fn draw_clearing(&self, strip: &mut StripBuffer, fonts: &SheetFonts) {
        let Some(job) = self.clearing.as_ref() else {
            return;
        };

        let mut meta = StackFmt::<64>::new();
        if job.images_total > 0 {
            let _ = write!(
                meta,
                "Removing images \u{00B7} {} of {}",
                job.images_done(),
                job.images_total
            );
        } else {
            let _ = meta.write_str("Removing the page index and covers");
        }
        let title = if job.title.is_empty() {
            job.filename.as_str()
        } else {
            job.title.as_str()
        };
        sheet::draw_header(strip, &self.geom, fonts, title, "CLEARING", meta.as_str());
        sheet::draw_groups(strip, &self.geom);

        let mut bundle_v = StackFmt::<20>::new();
        if job.bundle_freed {
            write_bytes(&mut bundle_v, job.bundle);
            let _ = bundle_v.write_str(" freed");
        } else {
            let _ = bundle_v.write_str("\u{2026}");
        }
        sheet::draw_row(
            strip,
            &self.geom,
            0,
            fonts,
            &RowSpec {
                lead: RowLead::None,
                text: "Text and layout",
                text_font: fonts.body,
                value: bundle_v.as_str(),
                selected: false,
                sub: "",
                progress: None,
                chip: ValueChip::None,
            },
        );

        let mut images_v = StackFmt::<20>::new();
        let _ = write!(images_v, "{} / {}", job.images_done(), job.images_total);
        sheet::draw_row(
            strip,
            &self.geom,
            1,
            fonts,
            &RowSpec {
                lead: RowLead::None,
                text: "Images",
                text_font: fonts.body,
                value: images_v.as_str(),
                selected: false,
                sub: "",
                progress: Some((job.images_done() as u32, job.images_total.max(1) as u32)),
                chip: ValueChip::None,
            },
        );

        sheet::draw_hints(strip, &self.geom, fonts, &[(HintSlot::Back, "STOP")]);
    }

    fn draw_done(&self, strip: &mut StripBuffer, fonts: &SheetFonts) {
        let Some((scope, title, freed)) = self.done else {
            return;
        };

        // the result is the meta line: e-paper has no way to fade a toast
        let mut meta = StackFmt::<64>::new();
        write_bytes(&mut meta, freed);
        let _ = meta.write_str(" freed \u{00B7} ");
        if self.count == 0 {
            let _ = meta.write_str("nothing left cached");
        } else {
            self.summary(&mut meta);
            let _ = meta.write_str(" left");
        }
        sheet::draw_header(
            strip,
            &self.geom,
            fonts,
            title.as_str(),
            scope.done_caption(),
            meta.as_str(),
        );
        sheet::draw_groups(strip, &self.geom);
        sheet::draw_row(
            strip,
            &self.geom,
            0,
            fonts,
            &RowSpec {
                lead: RowLead::Icon(sheet::ICON_LIST),
                text: "Back to the list",
                text_font: fonts.body,
                value: "\u{2192}",
                selected: true,
                sub: "",
                progress: None,
                chip: ValueChip::None,
            },
        );
        sheet::draw_hints(strip, &self.geom, fonts, &[(HintSlot::Back, "SETTINGS"), (HintSlot::Ok, "LIST")]);
    }

}

// ── formatting ──────────────────────────────────────────────────────

/// `412 KB`, `2.3 MB`, `88 B`. One decimal above a megabyte, none
/// below: the point of the number is which row to clear, not the byte.
fn write_bytes(out: &mut impl core::fmt::Write, bytes: u32) {
    const KB: u32 = 1024;
    const MB: u32 = 1024 * 1024;
    if bytes < KB {
        let _ = write!(out, "{} B", bytes);
    } else if bytes < MB {
        let _ = write!(out, "{} KB", (bytes + KB / 2) / KB);
    } else {
        let tenths = (bytes as u64 * 10 + MB as u64 / 2) / MB as u64;
        let _ = write!(out, "{}.{} MB", tenths / 10, tenths % 10);
    }
}

/// `9h 32m`, or minutes alone under the hour.
fn write_duration(out: &mut impl core::fmt::Write, secs: u32) {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        let _ = write!(out, "{}h {}m", hours, mins);
    } else {
        let _ = write!(out, "{}m", mins);
    }
}

/// The RECENT record's own filename capacity: a stack temporary, so
/// nothing here truncates before the comparison against the table.
const RECENT_NAME_CAP: usize = crate::apps::recent::FILENAME_CAP;

/// Filename the continue-reading card names, empty when there is none.
fn read_recent_filename(k: &mut KernelHandle<'_>) -> FixedStr<RECENT_NAME_CAP> {
    let mut buf = [0u8; crate::apps::recent::BUF_LEN];
    match k
        .sd()
        .read_file_start_in_dir(k.sd().data_dir(), RECENT_FILE, &mut buf)
    {
        Ok((_, n)) if n > 0 => FixedStr::from_bytes(RecentRecord::decode(&buf[..n]).filename),
        _ => FixedStr::EMPTY,
    }
}
