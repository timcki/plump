//! Minimal baseline JPEG decoder producing 1-bit Floyd–Steinberg dithered bitmaps.
//!
//! Streams MCU-row-by-row via 4 KB chunked reads; peak RAM ≈ 30 KB.
//! Luminance (Y) channel only — chrominance is Huffman-decoded to
//! advance the bitstream, then discarded.
//!
//! Progressive JPEG (SOF2) is rejected at decode time: multi-scan
//! progressive entropy streams cannot be reconstructed correctly
//! from a first-scan-only pass, so the decoder bails with a clear
//! error rather than producing degraded or corrupted output.
//!
//! Output is packed 1-bit MSB-first, row-major — see [`DecodedImage`](crate::DecodedImage).

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::DecodedImage;

// JPEG marker bytes

const M_SOF0: u8 = 0xC0;
const M_SOF2: u8 = 0xC2;
const M_DHT: u8 = 0xC4;
const M_SOI: u8 = 0xD8;
const M_EOI: u8 = 0xD9;
const M_SOS: u8 = 0xDA;
const M_DQT: u8 = 0xDB;
const M_DRI: u8 = 0xDD;
const M_RST0: u8 = 0xD0;
const M_RST7: u8 = 0xD7;

// limits

const MAX_COMP: usize = 4;
const MAX_PIXELS: u32 = 2048 * 2048;

// chunk size for streaming reads during MCU decode
const CHUNK_SIZE: usize = 4096;

// DEFLATE sliding-window size for streaming ZIP decompression
const DEFLATE_WINDOW: usize = 32768;

// zig-zag scan order

#[rustfmt::skip]
const ZZ: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

// IDCT constants (IJG ISLOW, CONST_BITS = 13)

const CB: i32 = 13;
const P1: i32 = 2;
const F0298: i32 = 2446;
const F0390: i32 = 3196;
const F0541: i32 = 4433;
const F0765: i32 = 6270;
const F0899: i32 = 7373;
const F1175: i32 = 9633;
const F1501: i32 = 12299;
const F1847: i32 = 15137;
const F1961: i32 = 16069;
const F2053: i32 = 16819;
const F2562: i32 = 20995;
const F3072: i32 = 25172;

// types

#[derive(Clone, Copy, Default)]
struct Component {
    id: u8,
    h_samp: u8,
    v_samp: u8,
    qt_idx: u8,
    dc_tbl: u8,
    ac_tbl: u8,
}

struct HuffTable {
    lut: [(u8, u8); 256],
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    values: [u8; 256],
}

struct JpegState {
    width: u16,
    height: u16,
    num_comp: u8,
    comp: [Component; MAX_COMP],
    max_h: u8,
    max_v: u8,
    qt: [[u16; 64]; 4],
    qt_ok: [bool; 4],
    dc_huff: [HuffTable; 4],
    ac_huff: [HuffTable; 4],
    dc_ok: [bool; 4],
    ac_ok: [bool; 4],
    restart_interval: u16,
    scan_num_comp: u8,
    scan_order: [u8; MAX_COMP],
    progressive: bool,
    // current-scan spectral selection start (0 = DC)
    scan_ss: u8,
    // current-scan spectral selection end (0 = DC only, 63 = all AC)
    scan_se: u8,
    // current-scan successive approximation high bit (0 = first scan,
    // !=0 = refinement of an earlier scan)
    scan_ah: u8,
    // current-scan successive approximation low bit (point transform)
    scan_al: u8,
    // logical position of the byte after the first SOS marker (FF DA);
    // used by the progressive band loop to seek back to entropy start.
    first_sos_offset: u32,
}

impl JpegState {
    fn heap_new() -> Result<Box<Self>, &'static str> {
        let layout = core::alloc::Layout::new::<Self>();
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err("jpeg: OOM for decoder state");
        }
        let mut st = unsafe { Box::from_raw(ptr as *mut Self) };
        st.max_h = 1;
        st.max_v = 1;
        for ht in st.dc_huff.iter_mut().chain(st.ac_huff.iter_mut()) {
            ht.maxcode.fill(-1);
        }
        Ok(st)
    }
}

// byte source trait + implementations

/// Byte-at-a-time JPEG byte source.
///
/// Used by [`decode_jpeg`] and [`peek_jpeg_dimensions`] to abstract
/// over in-memory slices, SD-backed chunked reads, and DEFLATE-decompressed
/// streams. Returning `Err` from `read_byte` is treated as an unexpected
/// EOF / I/O failure.
pub trait JpegRead {
    /// Read the next byte, or return an error on EOF / I/O failure.
    fn read_byte(&mut self) -> Result<u8, &'static str>;
    /// True when no more bytes are available from the source.
    fn is_eof(&self) -> bool;
    /// Logical byte offset of the next byte to be read. Starts at 0
    /// at construction and advances by 1 per `read_byte()`. Used by
    /// the progressive decoder to record the first-SOS offset and
    /// rewind to it for each band pass.
    fn position(&self) -> u32;
    /// Seek to absolute logical position `pos`. May seek backward
    /// (used by the progressive decoder); on sources with expensive
    /// reverse seeks (e.g. DEFLATE) this is supported but slow.
    /// Returns `Err` if `pos` is past the end of the stream.
    fn seek(&mut self, pos: u32) -> Result<(), &'static str>;
}

// allow &mut R to satisfy JpegRead so the progressive band loop can
// re-borrow the reader between scans (parse_sos + entropy decode +
// parse_sos + ...) without consuming it.
impl<R: JpegRead + ?Sized> JpegRead for &mut R {
    #[inline]
    fn read_byte(&mut self) -> Result<u8, &'static str> {
        (**self).read_byte()
    }
    #[inline]
    fn is_eof(&self) -> bool {
        (**self).is_eof()
    }
    #[inline]
    fn position(&self) -> u32 {
        (**self).position()
    }
    #[inline]
    fn seek(&mut self, pos: u32) -> Result<(), &'static str> {
        (**self).seek(pos)
    }
}

/// [`JpegRead`] backed by an in-memory slice.
pub struct SliceReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    /// Construct a reader over `data` starting at byte 0.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl JpegRead for SliceReader<'_> {
    #[inline]
    fn read_byte(&mut self) -> Result<u8, &'static str> {
        if self.pos >= self.data.len() {
            return Err("jpeg: unexpected end of data");
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    #[inline]
    fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    #[inline]
    fn position(&self) -> u32 {
        self.pos as u32
    }

    #[inline]
    fn seek(&mut self, pos: u32) -> Result<(), &'static str> {
        if (pos as usize) > self.data.len() {
            return Err("jpeg: seek past end of data");
        }
        self.pos = pos as usize;
        Ok(())
    }
}

/// [`JpegRead`] backed by a chunked random-access source (typically SD).
///
/// `read_fn(offset, buf)` is invoked to fill an internal 4 KB buffer
/// on demand; per-byte reads are served from that buffer. `start..end`
/// bounds the byte range available from the source.
pub struct ChunkReader<F> {
    read_fn: F,
    start: u32,  // absolute offset of logical position 0
    offset: u32, // absolute offset of next byte to fetch
    end: u32,    // end-of-data offset (exclusive)
    buf: [u8; CHUNK_SIZE],
    pos: usize,
    len: usize,
}

impl<F: FnMut(u32, &mut [u8]) -> Result<usize, &'static str>> ChunkReader<F> {
    /// Construct a reader covering bytes `[start, end)` of the source.
    pub fn new(read_fn: F, start: u32, end: u32) -> Self {
        Self {
            read_fn,
            start,
            offset: start,
            end,
            buf: [0u8; CHUNK_SIZE],
            pos: 0,
            len: 0,
        }
    }

    fn refill(&mut self) -> Result<(), &'static str> {
        if self.offset >= self.end {
            self.len = 0;
            return Ok(());
        }
        let want = CHUNK_SIZE.min((self.end - self.offset) as usize);
        let n = (self.read_fn)(self.offset, &mut self.buf[..want])?;
        if n == 0 {
            self.len = 0;
            return Ok(());
        }
        self.offset += n as u32;
        self.pos = 0;
        self.len = n;
        Ok(())
    }
}

impl<F: FnMut(u32, &mut [u8]) -> Result<usize, &'static str>> JpegRead for ChunkReader<F> {
    fn read_byte(&mut self) -> Result<u8, &'static str> {
        if self.pos >= self.len {
            self.refill()?;
            if self.len == 0 {
                return Err("jpeg: unexpected end of data");
            }
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.len && self.offset >= self.end
    }

    #[inline]
    fn position(&self) -> u32 {
        // self.buf[..self.len] currently holds source bytes
        // [self.offset - self.len, self.offset); next byte to deliver
        // sits at buffer index self.pos, so its source-absolute
        // position is offset - len + pos. logical position is
        // relative to self.start.
        (self.offset - self.len as u32) + self.pos as u32 - self.start
    }

    fn seek(&mut self, pos: u32) -> Result<(), &'static str> {
        let target = self
            .start
            .checked_add(pos)
            .ok_or("jpeg: seek offset overflow")?;
        if target > self.end {
            return Err("jpeg: seek past end of data");
        }
        self.offset = target;
        self.pos = 0;
        self.len = 0;
        Ok(())
    }
}

/// Streaming DEFLATE-from-source [`JpegRead`].
///
/// Decompresses a ZIP-stored DEFLATE blob on the fly, holding only a
/// 32 KB sliding window plus the miniz decompressor state (~47 KB peak).
/// Use this to decode JPEGs from `METHOD_DEFLATED` ZIP entries without
/// materialising the entire decompressed payload in memory.
pub struct DeflateReader<F> {
    read_fn: F,
    data_offset: u32, // original source offset (for rewind)
    comp_size: usize, // original compressed size (for rewind)
    file_pos: u32,    // absolute offset of next compressed byte
    comp_left: usize, // compressed bytes remaining in ZIP entry
    rbuf: Vec<u8>,    // compressed-data read buffer
    in_avail: usize,  // valid bytes in rbuf
    decomp: Box<miniz_oxide::inflate::core::DecompressorOxide>, // ~11KB
    window: Vec<u8>,  // 32KB circular dictionary
    dict_pos: usize,  // write position in window (cumulative, mod DEFLATE_WINDOW)
    read_pos: usize,  // next byte to yield from window
    avail: usize,     // decompressed bytes available (dict_pos - read_pos)
    done: bool,       // true once miniz reports Done
}

impl<F: FnMut(u32, &mut [u8]) -> Result<usize, &'static str>> DeflateReader<F> {
    /// Construct a reader over a DEFLATE-compressed blob at `data_offset`
    /// in the source, with `comp_size` compressed bytes available.
    pub fn new(read_fn: F, data_offset: u32, comp_size: u32) -> Result<Self, &'static str> {
        use miniz_oxide::inflate::core::DecompressorOxide;

        let decomp_ptr =
            unsafe { alloc::alloc::alloc_zeroed(core::alloc::Layout::new::<DecompressorOxide>()) };
        if decomp_ptr.is_null() {
            return Err("jpeg: OOM for DEFLATE decompressor");
        }
        let decomp = unsafe { Box::from_raw(decomp_ptr as *mut DecompressorOxide) };

        let mut window = Vec::new();
        window
            .try_reserve_exact(DEFLATE_WINDOW)
            .map_err(|_| "jpeg: OOM for DEFLATE window")?;
        window.resize(DEFLATE_WINDOW, 0);

        let mut rbuf = Vec::new();
        rbuf.try_reserve_exact(CHUNK_SIZE)
            .map_err(|_| "jpeg: OOM for DEFLATE read buffer")?;
        rbuf.resize(CHUNK_SIZE, 0);

        Ok(Self {
            read_fn,
            data_offset,
            comp_size: comp_size as usize,
            file_pos: data_offset,
            comp_left: comp_size as usize,
            rbuf,
            in_avail: 0,
            decomp,
            window,
            dict_pos: 0,
            read_pos: 0,
            avail: 0,
            done: false,
        })
    }

    // reset DEFLATE state to the start of the compressed stream.
    // used by `seek` for backward seeks; forward seeks fall through
    // to read_byte()-and-discard.
    fn reset(&mut self) {
        self.decomp.init();
        self.file_pos = self.data_offset;
        self.comp_left = self.comp_size;
        self.in_avail = 0;
        self.dict_pos = 0;
        self.read_pos = 0;
        self.avail = 0;
        self.done = false;
        // window contents are stale but irrelevant — miniz overwrites
        // via dict_pos as it produces output.
    }

    // decompress more data into the circular window
    fn pump(&mut self) -> Result<(), &'static str> {
        use miniz_oxide::inflate::TINFLStatus;
        use miniz_oxide::inflate::core::{decompress, inflate_flags};

        if self.done {
            return Ok(());
        }

        // top up read buffer from SD
        if self.in_avail < CHUNK_SIZE && self.comp_left > 0 {
            let space = CHUNK_SIZE - self.in_avail;
            let want = space.min(self.comp_left);
            match (self.read_fn)(
                self.file_pos,
                &mut self.rbuf[self.in_avail..self.in_avail + want],
            ) {
                Ok(n) if n > 0 => {
                    self.file_pos += n as u32;
                    self.comp_left -= n;
                    self.in_avail += n;
                }
                Ok(_) => {
                    self.comp_left = 0;
                }
                Err(e) => return Err(e),
            }
        }

        let flags = if self.comp_left > 0 {
            inflate_flags::TINFL_FLAG_HAS_MORE_INPUT
        } else {
            0
        };

        let write_pos = self.dict_pos & (DEFLATE_WINDOW - 1);
        let (status, consumed, produced) = decompress(
            &mut *self.decomp,
            &self.rbuf[..self.in_avail],
            &mut self.window,
            write_pos,
            flags,
        );

        if consumed > 0 && consumed < self.in_avail {
            self.rbuf.copy_within(consumed..self.in_avail, 0);
        }
        self.in_avail -= consumed;

        self.dict_pos += produced;
        self.avail += produced;

        match status {
            TINFLStatus::Done => {
                self.done = true;
            }
            TINFLStatus::HasMoreOutput | TINFLStatus::NeedsMoreInput => {}
            _ => return Err("jpeg: DEFLATE decompression error"),
        }

        Ok(())
    }
}

impl<F: FnMut(u32, &mut [u8]) -> Result<usize, &'static str>> JpegRead for DeflateReader<F> {
    fn read_byte(&mut self) -> Result<u8, &'static str> {
        if self.avail == 0 {
            if self.done {
                return Err("jpeg: unexpected end of DEFLATE stream");
            }
            self.pump()?;
            if self.avail == 0 {
                return Err("jpeg: unexpected end of DEFLATE stream");
            }
        }
        let rp = self.read_pos & (DEFLATE_WINDOW - 1);
        let b = self.window[rp];
        self.read_pos += 1;
        self.avail -= 1;
        Ok(b)
    }

    fn is_eof(&self) -> bool {
        self.avail == 0 && self.done
    }

    #[inline]
    fn position(&self) -> u32 {
        self.read_pos as u32
    }

    fn seek(&mut self, pos: u32) -> Result<(), &'static str> {
        let target = pos as usize;
        if target < self.read_pos {
            // backward seek: re-init DEFLATE state and replay forward.
            self.reset();
        }
        // forward-skip from current position to target by discarding
        // bytes. on a 32 KB sliding window this is cheap per byte but
        // costs O(n) memory accesses; only used by progressive decode
        // which is rare for DEFLATE-stored JPEGs.
        while self.read_pos < target {
            self.read_byte()?;
        }
        Ok(())
    }
}

// BitReader: generic over byte source

struct BitReader<R> {
    source: R,
    buf: u32,
    avail: u8,
    marker: u8, // stashed marker byte (non-zero = encountered during next_byte)
}

impl<R: JpegRead> BitReader<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            buf: 0,
            avail: 0,
            marker: 0,
        }
    }

    // fetch next entropy-coded byte, handling JPEG byte stuffing
    fn next_byte(&mut self) -> Result<u8, &'static str> {
        if self.marker != 0 {
            return Ok(0);
        }
        let b = self.source.read_byte()?;
        if b != 0xFF {
            return Ok(b);
        }
        loop {
            if self.source.is_eof() {
                return Ok(0);
            }
            let next = self.source.read_byte()?;
            match next {
                0x00 => return Ok(0xFF),
                0xFF => continue,
                _ => {
                    self.marker = next;
                    return Ok(0);
                }
            }
        }
    }

    fn ensure(&mut self, n: u8) -> Result<(), &'static str> {
        while self.avail < n {
            let b = self.next_byte()?;
            self.buf |= (b as u32) << (24 - self.avail);
            self.avail += 8;
        }
        Ok(())
    }

    #[inline]
    fn peek(&mut self, n: u8) -> Result<u32, &'static str> {
        self.ensure(n)?;
        Ok(self.buf >> (32 - n as u32))
    }

    #[inline]
    fn drop_bits(&mut self, n: u8) {
        self.buf <<= n as u32;
        self.avail -= n;
    }

    #[inline]
    fn read_bits(&mut self, n: u8) -> Result<u32, &'static str> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure(n)?;
        let val = self.buf >> (32 - n as u32);
        self.buf <<= n as u32;
        self.avail -= n;
        Ok(val)
    }

    // discard remaining bits, advance past the next restart marker
    fn consume_restart(&mut self) -> Result<(), &'static str> {
        self.buf = 0;
        self.avail = 0;

        // if next_byte already stashed a marker, check it now
        if self.marker != 0 {
            let m = self.marker;
            self.marker = 0;
            if m >= M_RST0 && m <= M_RST7 {
                return Ok(());
            }
            // non-RST marker; keep going
            return Ok(());
        }

        // scan forward for the restart marker
        loop {
            if self.source.is_eof() {
                return Ok(());
            }
            let b = self.source.read_byte()?;
            if b != 0xFF {
                continue;
            }
            loop {
                if self.source.is_eof() {
                    return Ok(());
                }
                let m = self.source.read_byte()?;
                match m {
                    0xFF => continue,
                    0x00 => break,
                    M_RST0..=M_RST7 => return Ok(()),
                    _ => return Ok(()),
                }
            }
        }
    }
}

// public API

/// Decode a JPEG to a 1-bit dithered bitmap from any [`JpegRead`].
///
/// The image is integer-downscaled so the result fits within
/// `max_w` × `max_h` pixels. Both baseline (SOF0) and progressive
/// (SOF2) JPEGs are supported. The reader is consumed; on success,
/// all bytes up to (but not necessarily including) the EOI have
/// been read from the source.
///
/// Progressive decode buffers a band of MCU rows at a time and
/// re-reads the bitstream once per band; peak RAM stays bounded
/// at the cost of slower decode for large images.
pub fn decode_jpeg<R: JpegRead>(
    mut reader: R,
    max_w: u16,
    max_h: u16,
) -> Result<DecodedImage, &'static str> {
    let st = parse_markers(&mut reader)?;
    validate_tables(&st)?;
    if st.progressive {
        decode_progressive(st, reader, max_w, max_h)
    } else {
        decode_baseline(&st, BitReader::new(reader), max_w, max_h)
    }
}

/// Read the dimensions of a JPEG without performing a full decode.
///
/// Scans markers until SOF0 / SOF2 and returns `(width, height)` in
/// source pixels.
pub fn peek_jpeg_dimensions<R: JpegRead>(
    mut reader: R,
) -> Result<(u16, u16), &'static str> {
    scan_sof_streaming(&mut reader)
}

// ── baseline decode core (generic over byte source) ────────────────

fn validate_tables(st: &JpegState) -> Result<(), &'static str> {
    for sci in 0..st.scan_num_comp as usize {
        let ci = st.scan_order[sci] as usize;
        let c = &st.comp[ci];
        if !st.qt_ok[c.qt_idx as usize] {
            return Err("jpeg: missing quant table");
        }
        if !st.dc_ok[c.dc_tbl as usize] {
            return Err("jpeg: missing DC Huffman table");
        }
        if st.scan_se > 0 && !st.ac_ok[c.ac_tbl as usize] {
            return Err("jpeg: missing AC Huffman table");
        }
    }
    Ok(())
}

/// Horizontally-binned buffer for one MCU row of luma.
///
/// The decoder downscales by an integer `scale`, so only `out_w` of the
/// `row_w` source columns reach the output. Materialising the band at
/// full source width and then point-sampling one column in `scale` cost
/// `row_w * mcu_h` bytes — 53 KB for a 6776 px wide image — and dropped
/// any feature narrower than `scale` with probability `1 - width/scale`.
///
/// Instead each decoded block folds its pixels into the output column
/// they land in, so the band is `out_w` wide and every source pixel
/// contributes. That is both far smaller and a proper box filter: thin
/// strokes survive as grey rather than appearing or vanishing depending
/// on where they fell relative to the sampling grid.
///
/// `scale == 1` keeps byte-per-column storage, since binning would only
/// widen each entry to `u16` for no gain on an image already inside the
/// budget.
struct Band {
    // exactly one of these is non-empty
    direct: Vec<u8>,   // scale == 1: source pixels
    binned: Vec<u16>,  // scale >= 2: per-row horizontal sums
    /// source columns folded into each output column. Zero where no
    /// block covers the bin, which renders mid-grey the way the old
    /// full-width `fill(128)` did.
    col_n: Vec<u16>,
    /// right shift applied to each sample before it enters `binned`,
    /// so `scale * 255` cannot overflow a `u16` at extreme reductions.
    shift: u8,
    out_w: usize,
    scale: usize,
}

impl Band {
    fn new(
        st: &JpegState,
        w: usize,
        scale: usize,
        out_w: usize,
        mcu_w: usize,
        mcu_h: usize,
        mcus_x: usize,
    ) -> Result<Self, &'static str> {
        // widest bin holds `scale` samples; keep the running sum inside u16
        let mut shift = 0u8;
        while ((scale >> shift) * 255) > u16::MAX as usize {
            shift += 1;
        }

        let mut col_n: Vec<u16> = Vec::new();
        col_n
            .try_reserve_exact(out_w)
            .map_err(|_| "jpeg: OOM for band")?;
        col_n.resize(out_w, 0);
        // walk the luma block grid once to learn how many source columns
        // actually reach each bin: `scale` in the interior, fewer at the
        // right edge, zero for bins no block covers
        for mx in 0..mcus_x {
            for bh in 0..st.comp[0].h_samp as usize {
                let bx = mx * mcu_w + bh * 8;
                for c in 0..8 {
                    let sx = bx + c;
                    if sx >= w {
                        break;
                    }
                    let ox = sx / scale;
                    if ox < out_w {
                        col_n[ox] += 1;
                    }
                }
            }
        }

        let mut direct = Vec::new();
        let mut binned = Vec::new();
        if scale == 1 {
            direct
                .try_reserve_exact(out_w * mcu_h)
                .map_err(|_| "jpeg: OOM for band")?;
            direct.resize(out_w * mcu_h, 0u8);
        } else {
            binned
                .try_reserve_exact(out_w * mcu_h)
                .map_err(|_| "jpeg: OOM for band")?;
            binned.resize(out_w * mcu_h, 0u16);
        }

        Ok(Self {
            direct,
            binned,
            col_n,
            shift,
            out_w,
            scale,
        })
    }

    fn clear(&mut self) {
        self.direct.fill(0);
        self.binned.fill(0);
    }

    /// Fold one IDCT'd 8x8 luma block into the band. `bx` / `by` are the
    /// block's source column / band-relative row.
    fn scatter(&mut self, bx: usize, by: usize, w: usize, pix: &[u8; 64]) {
        if self.scale == 1 {
            // bins map 1:1 to source columns; clamp the MCU-grid overhang
            // that used to land in the full-width band's padding
            let n = 8.min(self.out_w.saturating_sub(bx));
            if n == 0 {
                return;
            }
            for r in 0..8 {
                let dst = (by + r) * self.out_w + bx;
                self.direct[dst..dst + n].copy_from_slice(&pix[r * 8..r * 8 + n]);
            }
            return;
        }

        // the block's 8 columns span at most two bins, so resolve them
        // once here and reuse the mapping across all 8 rows
        let mut bin = [usize::MAX; 8];
        for (c, slot) in bin.iter_mut().enumerate() {
            let sx = bx + c;
            if sx >= w {
                break;
            }
            let ox = sx / self.scale;
            if ox < self.out_w {
                *slot = ox;
            }
        }

        for r in 0..8 {
            let base = (by + r) * self.out_w;
            let src = &pix[r * 8..r * 8 + 8];
            for c in 0..8 {
                let ox = bin[c];
                if ox == usize::MAX {
                    continue;
                }
                self.binned[base + ox] += (src[c] >> self.shift) as u16;
            }
        }
    }

    /// Add band row `py` into the vertical accumulator.
    fn fold_row(&self, py: usize, acc: &mut [u32]) {
        let lo = py * self.out_w;
        if self.scale == 1 {
            for (a, &v) in acc.iter_mut().zip(&self.direct[lo..lo + self.out_w]) {
                *a += v as u32;
            }
        } else {
            for (a, &v) in acc.iter_mut().zip(&self.binned[lo..lo + self.out_w]) {
                *a += v as u32;
            }
        }
    }
}

fn decode_baseline<R: JpegRead>(
    st: &JpegState,
    mut reader: BitReader<R>,
    max_w: u16,
    max_h: u16,
) -> Result<DecodedImage, &'static str> {
    let w = st.width as usize;
    let h = st.height as usize;
    if w == 0 || h == 0 {
        return Err("jpeg: zero dimensions");
    }
    if (w as u32).saturating_mul(h as u32) > MAX_PIXELS {
        return Err("jpeg: exceeds pixel limit");
    }

    let scale = {
        let sw = (w + max_w as usize - 1) / max_w as usize;
        let sh = (h + max_h as usize - 1) / max_h as usize;
        sw.max(sh).max(1)
    };
    let out_w = (w / scale).max(1);
    let out_h = (h / scale).max(1);
    let out_stride = (out_w + 7) / 8;

    let mcu_w = st.max_h as usize * 8;
    let mcu_h = st.max_v as usize * 8;
    let mcus_x = (w + mcu_w - 1) / mcu_w;
    let mcus_y = (h + mcu_h - 1) / mcu_h;

    log::info!(
        "jpeg: baseline {}x{} → {}x{} (scale {})",
        w,
        h,
        out_w,
        out_h,
        scale
    );

    // allocate buffers

    // output plane first: it is the biggest single reservation here
    // and the only one that outlives the decode, so it gets the pick
    // of the heap before the band splits a region it could have used
    let mut output = Vec::new();
    output
        .try_reserve_exact(out_stride * out_h)
        .map_err(|_| "jpeg: OOM for output")?;
    output.resize(out_stride * out_h, 0u8);
    let mut band = Band::new(st, w, scale, out_w, mcu_w, mcu_h, mcus_x)?;
    let mut acc_row: Vec<u32> = Vec::new();
    acc_row
        .try_reserve_exact(out_w)
        .map_err(|_| "jpeg: OOM for band")?;
    acc_row.resize(out_w, 0);
    let mut err_cur = Vec::new();
    err_cur
        .try_reserve_exact(out_w + 2)
        .map_err(|_| "jpeg: OOM for dither")?;
    err_cur.resize(out_w + 2, 0i16);
    let mut err_nxt = Vec::new();
    err_nxt
        .try_reserve_exact(out_w + 2)
        .map_err(|_| "jpeg: OOM for dither")?;
    err_nxt.resize(out_w + 2, 0i16);

    let mut dc_pred = [0i32; MAX_COMP];
    let mut block = [0i32; 64];
    let mut pix = [0u8; 64];
    let mut mcu_cnt: u32 = 0;
    let total_mcus = (mcus_x * mcus_y) as u32;
    let mut out_y: usize = 0;

    // MCU decode loop

    let mut rows_in_acc: u32 = 0;

    for mcu_row in 0..mcus_y {
        band.clear();

        for mcu_col in 0..mcus_x {
            for sci in 0..st.scan_num_comp as usize {
                let ci = st.scan_order[sci] as usize;
                let c = &st.comp[ci];
                let is_y = ci == 0;

                for bv in 0..c.v_samp as usize {
                    for bh in 0..c.h_samp as usize {
                        if is_y {
                            decode_block(
                                &mut reader,
                                &st.dc_huff[c.dc_tbl as usize],
                                &st.ac_huff[c.ac_tbl as usize],
                                &mut dc_pred[ci],
                                &st.qt[c.qt_idx as usize],
                                &mut block,
                                st.scan_se as usize,
                                st.scan_al,
                            )?;
                            idct(&block, &mut pix);
                            band.scatter(mcu_col * mcu_w + bh * 8, bv * 8, w, &pix);
                        } else {
                            skip_block(
                                &mut reader,
                                &st.dc_huff[c.dc_tbl as usize],
                                &st.ac_huff[c.ac_tbl as usize],
                                &mut dc_pred[ci],
                                st.scan_se as usize,
                            )?;
                        }
                    }
                }
            }

            mcu_cnt += 1;

            if st.restart_interval > 0
                && mcu_cnt % st.restart_interval as u32 == 0
                && mcu_cnt < total_mcus
            {
                reader.consume_restart()?;
                dc_pred.fill(0);
            }
        }

        // fold this MCU row into the output grid, emitting a dithered
        // row each time `scale` source rows have accumulated
        for py in 0..mcu_h {
            let src_y = mcu_row * mcu_h + py;
            // rows past the last whole output row are dropped, as they
            // were when only every `scale`-th row was sampled
            if src_y >= h || src_y / scale >= out_h {
                break;
            }
            band.fold_row(py, &mut acc_row);
            rows_in_acc += 1;
            if rows_in_acc as usize == scale {
                emit_row(
                    &acc_row,
                    &band,
                    rows_in_acc,
                    &mut err_cur,
                    &mut err_nxt,
                    &mut output[out_y * out_stride..(out_y + 1) * out_stride],
                );
                out_y += 1;
                core::mem::swap(&mut err_cur, &mut err_nxt);
                err_nxt.fill(0);
                acc_row.fill(0);
                rows_in_acc = 0;
            }
        }
    }

    // an image shorter than one output row (h < scale) leaves a partial
    // accumulation with nothing to complete it
    if rows_in_acc > 0 && out_y < out_h {
        emit_row(
            &acc_row,
            &band,
            rows_in_acc,
            &mut err_cur,
            &mut err_nxt,
            &mut output[out_y * out_stride..(out_y + 1) * out_stride],
        );
        out_y += 1;
    }

    Ok(DecodedImage {
        width: out_w as u16,
        height: out_y as u16,
        data: output,
        stride: out_stride,
    })
}

// ── progressive decode (multi-scan, multi-pass) ────────────────────
//
// progressive JPEGs encode each MCU's coefficients across several
// scans (DC first, then AC band by band, optionally refined by
// successive approximation). a single forward pass cannot emit
// pixels because later scans refine earlier coefficients; we must
// hold quantized coefficients resident until the last refinement
// for each MCU.
//
// to stay inside the e-reader's heap budget we band the image
// vertically: K MCU rows of Y coefficients are buffered at a time,
// and the bitstream is re-read from `first_sos_offset` once per
// band. for each pass, only Y blocks whose MCU row falls inside
// the current band are stored; out-of-band Y blocks and chroma
// blocks are entropy-decoded into a scratch slot and discarded.
//
// when the band's scans are exhausted, coefficients are
// dequantized and IDCT'd into Y pixels, then the band's rows
// are streamed through the same Floyd-Steinberg pipeline as
// baseline.

// budget for the band coefficient buffer (i16, raw quantized).
// this is a target, not a floor: decode_progressive halves K until
// the reservation fits. 48 KB keeps the decode transient small
// enough to coexist with the reader's typeset allocations on a
// ~160 KB heap (96 KB bands drove the combined peak to ~157 KB and
// starved concurrent Vec growth); the cost is proportionally more
// passes over the entropy stream per image.
const MAX_BAND_BYTES: usize = 48 * 1024;

#[inline]
fn band_block_offset(
    my: usize,
    mx: usize,
    bidx: usize,
    mcus_x: usize,
    y_blocks_per_mcu: usize,
) -> usize {
    (((my * mcus_x) + mx) * y_blocks_per_mcu + bidx) * 64
}

// DC first scan: read DC differential, scale by Al, store at coeff 0
fn pdc_first<R: JpegRead>(
    r: &mut BitReader<R>,
    dc_ht: &HuffTable,
    dc_pred: &mut i32,
    al: u8,
    blk: &mut [i16],
) -> Result<(), &'static str> {
    let dc_size = huff_decode(r, dc_ht)?;
    if dc_size > 0 {
        if dc_size > 11 {
            return Err("jpeg: DC size > 11");
        }
        let bits = r.read_bits(dc_size)?;
        *dc_pred += extend(bits, dc_size);
    }
    blk[0] = ((*dc_pred) << al) as i16;
    Ok(())
}

// DC refinement: 1 bit, OR'd into existing DC at bit Al
fn pdc_refine<R: JpegRead>(
    r: &mut BitReader<R>,
    al: u8,
    blk: &mut [i16],
) -> Result<(), &'static str> {
    let bit = r.read_bits(1)?;
    if bit != 0 {
        blk[0] |= 1i16 << al;
    }
    Ok(())
}

// AC first scan: RLE+Huffman over coefficients [ss..=se], scaled by Al,
// with EOBRUN handling for runs of all-zero blocks.
fn pac_first<R: JpegRead>(
    r: &mut BitReader<R>,
    ac_ht: &HuffTable,
    ss: usize,
    se: usize,
    al: u8,
    eobrun: &mut u16,
    blk: &mut [i16],
) -> Result<(), &'static str> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = ss;
    while k <= se {
        let sym = huff_decode(r, ac_ht)?;
        let run = (sym >> 4) as usize;
        let size = sym & 0x0F;
        if size == 0 {
            if run == 15 {
                k += 16;
            } else {
                // EOBn: 1<<run + read run bits
                let bits = if run > 0 { r.read_bits(run as u8)? } else { 0 };
                let count = (1u32 << run) + bits;
                *eobrun = (count - 1) as u16;
                break;
            }
        } else {
            k += run;
            if k > se {
                return Err("jpeg: AC index overflow (first)");
            }
            let bits = r.read_bits(size)?;
            let val = extend(bits, size) as i16;
            blk[ZZ[k]] = val << al;
            k += 1;
        }
    }
    Ok(())
}

// AC refinement scan: refines existing nonzero coefficients in
// [ss..=se] by setting bit Al; new nonzero coefficients are
// introduced as ±(1<<Al) at positions where the run skipped past
// zeros. EOBRUN handling: when an EOBn symbol is read, this block's
// remaining nonzeros still get refinement bits, then `eobrun` is
// armed with N-1 (so the next N-1 blocks are walked in the
// "during EOBRUN" branch).
fn pac_refine<R: JpegRead>(
    r: &mut BitReader<R>,
    ac_ht: &HuffTable,
    ss: usize,
    se: usize,
    al: u8,
    eobrun: &mut u16,
    blk: &mut [i16],
) -> Result<(), &'static str> {
    let p1: i16 = 1i16 << al;
    let m1: i16 = (-1i16) << al;

    // helper: refine the existing nonzero at zigzag index k by
    // reading one refinement bit. returns Ok regardless of value;
    // the caller advances k.
    fn refine_existing<R: JpegRead>(
        r: &mut BitReader<R>,
        blk: &mut [i16],
        pos: usize,
        p1: i16,
        m1: i16,
    ) -> Result<(), &'static str> {
        if r.read_bits(1)? != 0 {
            if (blk[pos] & p1) == 0 {
                if blk[pos] >= 0 {
                    blk[pos] += p1;
                } else {
                    blk[pos] += m1;
                }
            }
        }
        Ok(())
    }

    if *eobrun > 0 {
        // not the EOB block; walk the band refining existing nonzeros
        // and decrement the EOBRUN counter.
        let mut k = ss;
        while k <= se {
            let pos = ZZ[k];
            if blk[pos] != 0 {
                refine_existing(r, blk, pos, p1, m1)?;
            }
            k += 1;
        }
        *eobrun -= 1;
        return Ok(());
    }

    let mut k = ss;
    while k <= se {
        let sym = huff_decode(r, ac_ht)?;
        let run = (sym >> 4) as usize;
        let size = sym & 0x0F;

        let new_val: i16;
        if size == 0 {
            if run != 15 {
                // EOBn: refine this block's remaining nonzeros, then
                // arm EOBRUN with the count - 1 (this block consumes
                // one of the N).
                let bits = if run > 0 { r.read_bits(run as u8)? } else { 0 };
                let count = (1u32 << run) + bits;
                while k <= se {
                    let pos = ZZ[k];
                    if blk[pos] != 0 {
                        refine_existing(r, blk, pos, p1, m1)?;
                    }
                    k += 1;
                }
                *eobrun = (count - 1) as u16;
                return Ok(());
            }
            // ZRL: 16 zero coefficients to skip; no new value placed.
            new_val = 0;
        } else {
            if size != 1 {
                return Err("jpeg: bad refinement size");
            }
            new_val = if r.read_bits(1)? == 0 { m1 } else { p1 };
        }

        // walk forward, refining each existing nonzero and counting
        // down `run` zero coefficients. when run reaches 0, the next
        // zero is where new_val (if non-zero) belongs.
        let mut zerorun = run as i32;
        loop {
            if k > se {
                if new_val == 0 {
                    // ZRL ran past end with no remaining nonzeros to
                    // place — accept.
                    return Ok(());
                }
                log::warn!(
                    "jpeg: AC refine overflow ss={} se={} k={} run={} size={} sym={:#x}",
                    ss,
                    se,
                    k,
                    run,
                    size,
                    sym,
                );
                // TODO(progressive): some real-world progressive JPEGs
                // hit this path during the high-bit AC refinement scan
                // (Ah > 0). the failure pattern is a "place new ±1"
                // symbol arriving when no zero remains in [k..=Se],
                // suggesting a state mismatch with prior scans. for
                // now we error so callers fall back to alt text rather
                // than caching a corrupted preview.
                return Err("jpeg: AC refinement overflow");
            }
            let pos = ZZ[k];
            if blk[pos] != 0 {
                refine_existing(r, blk, pos, p1, m1)?;
                k += 1;
            } else {
                if zerorun == 0 {
                    if new_val != 0 {
                        blk[pos] = new_val;
                    }
                    k += 1;
                    break;
                }
                zerorun -= 1;
                k += 1;
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_progressive_block<R: JpegRead>(
    r: &mut BitReader<R>,
    dc_ht: &HuffTable,
    ac_ht: &HuffTable,
    dc_pred: &mut i32,
    eobrun: &mut u16,
    ss: usize,
    se: usize,
    ah: u8,
    al: u8,
    blk: &mut [i16],
) -> Result<(), &'static str> {
    if ss == 0 {
        // DC scan
        if ah == 0 {
            pdc_first(r, dc_ht, dc_pred, al, blk)
        } else {
            pdc_refine(r, al, blk)
        }
    } else {
        // AC scan
        if ah == 0 {
            pac_first(r, ac_ht, ss, se, al, eobrun, blk)
        } else {
            pac_refine(r, ac_ht, ss, se, al, eobrun, blk)
        }
    }
}

// skim entropy bytes of a scan we don't need (chroma AC refinement).
// reads raw bytes from the source, handling FF 00 stuffing and RSTn
// markers, until a non-RST marker is found. returns that marker.
fn skim_scan_to_marker<R: JpegRead>(reader: &mut R) -> Result<u8, &'static str> {
    loop {
        let b = reader.read_byte()?;
        if b != 0xFF {
            continue;
        }
        loop {
            let m = reader.read_byte()?;
            match m {
                0x00 => break,                    // byte stuffing — emitted as 0xFF data
                0xFF => continue,                 // fill byte
                M_RST0..=M_RST7 => break,         // mid-scan restart marker
                _ => return Ok(m),                // real marker — scan is over
            }
        }
    }
}

// walk one progressive scan to its terminating marker. returns the
// non-RST marker code that ended the scan (M_SOS for next scan,
// M_EOI for end of file).
#[allow(clippy::too_many_arguments)]
fn walk_progressive_scan<R: JpegRead>(
    bit: &mut BitReader<&mut R>,
    st: &JpegState,
    band_y: &mut [i16],
    band_top: usize,
    band_bottom: usize,
    mcus_x: usize,
    mcus_y: usize,
    y_blocks_per_mcu: usize,
) -> Result<u8, &'static str> {
    let mut dc_pred = [0i32; MAX_COMP];
    let mut eobrun: u16 = 0;
    let mut scratch = [0i16; 64];

    let ss = st.scan_ss as usize;
    let se = st.scan_se as usize;
    let ah = st.scan_ah;
    let al = st.scan_al;
    let restart_interval = st.restart_interval as u32;

    let interleaved = st.scan_num_comp > 1;

    // helper: pull a block target slice for the given MCU/component;
    // returns a scratch slice if outside the band or non-Y component.
    if interleaved {
        let mut mcu_cnt: u32 = 0;
        let total_mcus = (mcus_x * mcus_y) as u32;
        for my in 0..mcus_y {
            let in_band = my >= band_top && my < band_bottom;
            let band_my = my.wrapping_sub(band_top);
            for mx in 0..mcus_x {
                for sci in 0..st.scan_num_comp as usize {
                    let ci = st.scan_order[sci] as usize;
                    let c = &st.comp[ci];
                    let dc_ht = &st.dc_huff[c.dc_tbl as usize];
                    let ac_ht = &st.ac_huff[c.ac_tbl as usize];
                    for bv in 0..c.v_samp as usize {
                        for bh in 0..c.h_samp as usize {
                            let blk: &mut [i16] = if ci == 0 && in_band {
                                let bidx = bv * c.h_samp as usize + bh;
                                let off = band_block_offset(
                                    band_my,
                                    mx,
                                    bidx,
                                    mcus_x,
                                    y_blocks_per_mcu,
                                );
                                &mut band_y[off..off + 64]
                            } else {
                                &mut scratch
                            };
                            decode_progressive_block(
                                bit,
                                dc_ht,
                                ac_ht,
                                &mut dc_pred[ci],
                                &mut eobrun,
                                ss,
                                se,
                                ah,
                                al,
                                blk,
                            )?;
                        }
                    }
                }
                mcu_cnt += 1;
                if restart_interval > 0
                    && mcu_cnt % restart_interval == 0
                    && mcu_cnt < total_mcus
                {
                    bit.consume_restart()?;
                    dc_pred.fill(0);
                    eobrun = 0;
                }
            }
        }
    } else {
        // non-interleaved single-component scan: data units are blocks
        // of the single component, in raster order across the
        // component's OWN block grid: ceil(comp_samples/8) per axis
        // (JPEG A.2.2), NOT the MCU-padded grid. for 4:2:0 Y the MCU
        // grid is up to one block wider/taller; walking it over-reads
        // the entropy stream and desyncs every later block (seen as
        // "invalid Huffman code" on real files)
        let ci = st.scan_order[0] as usize;
        let c = &st.comp[ci];
        let dc_ht = &st.dc_huff[c.dc_tbl as usize];
        let ac_ht = &st.ac_huff[c.ac_tbl as usize];
        let comp_w = (st.width as usize * c.h_samp as usize).div_ceil(st.max_h as usize);
        let comp_h = (st.height as usize * c.v_samp as usize).div_ceil(st.max_v as usize);
        let blocks_x = comp_w.div_ceil(8);
        let blocks_y = comp_h.div_ceil(8);
        let total_blocks = (blocks_x * blocks_y) as u32;
        let mut blk_cnt: u32 = 0;
        for cy in 0..blocks_y {
            let my = cy / c.v_samp as usize;
            let bv = cy % c.v_samp as usize;
            let in_band = my >= band_top && my < band_bottom;
            let band_my = my.wrapping_sub(band_top);
            for cx in 0..blocks_x {
                let mx = cx / c.h_samp as usize;
                let bh = cx % c.h_samp as usize;
                let blk: &mut [i16] = if ci == 0 && in_band {
                    let bidx = bv * c.h_samp as usize + bh;
                    let off = band_block_offset(
                        band_my,
                        mx,
                        bidx,
                        mcus_x,
                        y_blocks_per_mcu,
                    );
                    &mut band_y[off..off + 64]
                } else {
                    &mut scratch
                };
                decode_progressive_block(
                    bit,
                    dc_ht,
                    ac_ht,
                    &mut dc_pred[ci],
                    &mut eobrun,
                    ss,
                    se,
                    ah,
                    al,
                    blk,
                )?;
                blk_cnt += 1;
                if restart_interval > 0
                    && blk_cnt % restart_interval == 0
                    && blk_cnt < total_blocks
                {
                    bit.consume_restart()?;
                    dc_pred.fill(0);
                    eobrun = 0;
                }
            }
        }
    }

    // entropy ended; the terminating marker should be stashed in
    // bit.marker by the FF-handling path inside next_byte.
    if bit.marker == 0 {
        // scan ran off the end without a marker — search forward for
        // any FF xx (excluding RSTn / 0xFF fill / 0x00 stuffing) so the
        // caller can decide what to do with it.
        loop {
            if bit.source.is_eof() {
                return Err("jpeg: scan ended without terminating marker");
            }
            let b = bit.source.read_byte()?;
            if b != 0xFF {
                continue;
            }
            loop {
                if bit.source.is_eof() {
                    return Err("jpeg: scan ended without terminating marker");
                }
                let m = bit.source.read_byte()?;
                match m {
                    0xFF => continue,
                    0x00 => break,
                    M_RST0..=M_RST7 => break,
                    _ => {
                        bit.marker = m;
                        break;
                    }
                }
            }
            if bit.marker != 0 {
                break;
            }
        }
    }
    let m = bit.marker;
    bit.marker = 0;
    Ok(m)
}

fn decode_progressive<R: JpegRead>(
    mut st: Box<JpegState>,
    mut reader: R,
    max_w: u16,
    max_h: u16,
) -> Result<DecodedImage, &'static str> {
    let w = st.width as usize;
    let h = st.height as usize;
    if w == 0 || h == 0 {
        return Err("jpeg: zero dimensions");
    }
    if (w as u32).saturating_mul(h as u32) > MAX_PIXELS {
        return Err("jpeg: exceeds pixel limit");
    }

    let scale = {
        let sw = (w + max_w as usize - 1) / max_w as usize;
        let sh = (h + max_h as usize - 1) / max_h as usize;
        sw.max(sh).max(1)
    };
    let out_w = (w / scale).max(1);
    let out_h = (h / scale).max(1);
    let out_stride = (out_w + 7) / 8;

    let mcu_w = st.max_h as usize * 8;
    let mcu_h = st.max_v as usize * 8;
    let mcus_x = (w + mcu_w - 1) / mcu_w;
    let mcus_y = (h + mcu_h - 1) / mcu_h;

    // Y component is comp[0]; blocks-per-MCU for Y equals max_h * max_v
    // (Y is by convention the highest-sampled component, so its samples
    // match the MCU dimensions).
    let y_blocks_per_mcu = st.comp[0].h_samp as usize * st.comp[0].v_samp as usize;
    let bytes_per_mcu_row = mcus_x * y_blocks_per_mcu * 64 * 2;
    if bytes_per_mcu_row == 0 {
        return Err("jpeg: empty MCU row");
    }
    let mut k = MAX_BAND_BYTES / bytes_per_mcu_row;
    if k == 0 {
        k = 1;
    }
    if k > mcus_y {
        k = mcus_y;
    }

    // output plane first: it is the biggest single reservation here
    // and the only one that outlives the decode, so it gets the pick
    // of the heap before the band splits a region it could have used
    let mut output = Vec::new();
    output
        .try_reserve_exact(out_stride * out_h)
        .map_err(|_| "jpeg: OOM for output")?;
    output.resize(out_stride * out_h, 0u8);
    let mut band = Band::new(&st, w, scale, out_w, mcu_w, mcu_h, mcus_x)?;
    let mut acc_row: Vec<u32> = Vec::new();
    acc_row
        .try_reserve_exact(out_w)
        .map_err(|_| "jpeg: OOM for band")?;
    acc_row.resize(out_w, 0);
    let mut err_cur = Vec::new();
    err_cur
        .try_reserve_exact(out_w + 2)
        .map_err(|_| "jpeg: OOM for dither")?;
    err_cur.resize(out_w + 2, 0i16);
    let mut err_nxt = Vec::new();
    err_nxt
        .try_reserve_exact(out_w + 2)
        .map_err(|_| "jpeg: OOM for dither")?;
    err_nxt.resize(out_w + 2, 0i16);

    // band buffer (i16 quantized coefficients for Y blocks of K MCU rows).
    // allocated adaptively: MAX_BAND_BYTES is a target, not a requirement,
    // so halve K until the reservation succeeds; smaller bands only cost
    // extra passes over the entropy stream.  it comes last because it is
    // the only buffer here that can shrink: taking the 48 KB target before
    // the fixed-size output starved that allocation on a tight heap
    let mut band_y: Vec<i16> = Vec::new();
    loop {
        let band_len = k * mcus_x * y_blocks_per_mcu * 64;
        if band_y.try_reserve_exact(band_len).is_ok() {
            band_y.resize(band_len, 0);
            break;
        }
        if k == 1 {
            return Err("jpeg: OOM for progressive band");
        }
        k /= 2;
    }

    log::info!(
        "jpeg: progressive {}x{} → {}x{} (scale {}, K={}, band {} KB)",
        w,
        h,
        out_w,
        out_h,
        scale,
        k,
        (k * bytes_per_mcu_row) / 1024
    );

    let first_sos = st.first_sos_offset;
    let mut block_i32 = [0i32; 64];
    let mut pix = [0u8; 64];
    let mut out_y: usize = 0;
    let mut rows_in_acc: u32 = 0;

    let band_count = (mcus_y + k - 1) / k;
    for band_idx in 0..band_count {
        let band_top = band_idx * k;
        let band_bottom = (band_top + k).min(mcus_y);

        // clear the band buffer; coefficients accumulate from zero
        // across the scans of this pass.
        let used = (band_bottom - band_top) * mcus_x * y_blocks_per_mcu * 64;
        band_y[..used].fill(0);

        // walk every scan in the file, seeking to the first SOS at
        // the start of each band.
        reader.seek(first_sos)?;
        let mut scan_idx = 0usize;
        loop {
            // we just consumed FF DA (either by parse_markers initially
            // or by the previous scan's BitReader); parse the SOS
            // segment header now.
            parse_sos(&mut reader, &mut st)?;
            log::debug!(
                "jpeg: band {} scan {}: ss={} se={} ah={} al={} ncomp={}",
                band_idx,
                scan_idx,
                st.scan_ss,
                st.scan_se,
                st.scan_ah,
                st.scan_al,
                st.scan_num_comp,
            );
            scan_idx += 1;

            // skim AC refinement scans whose blocks we can't refine
            // with correct state: pac_refine reads existing nonzeros
            // to determine the refinement bit count, so any block
            // whose first-scan coefficients aren't resident corrupts
            // the bitstream. we skim:
            //   * any chroma AC refinement (we don't store chroma)
            //   * Y AC refinement when K < mcus_y (out-of-band Y
            //     blocks fall through to scratch, which has stale
            //     state from prior scans)
            // DC refinement reads exactly 1 bit per block regardless
            // of state, so the desync doesn't apply there.
            let is_ac_refine = st.scan_num_comp == 1
                && st.scan_ah != 0
                && st.scan_ss != 0;
            let needs_skim = is_ac_refine
                && (st.scan_order[0] != 0 || k < mcus_y);
            let marker = if needs_skim {
                log::debug!(
                    "jpeg: skim ac-refine scan ci={} (k={} mcus_y={})",
                    st.scan_order[0],
                    k,
                    mcus_y,
                );
                skim_scan_to_marker(&mut reader)?
            } else {
                let mut bit = BitReader::new(&mut reader);
                walk_progressive_scan(
                    &mut bit,
                    &st,
                    &mut band_y,
                    band_top,
                    band_bottom,
                    mcus_x,
                    mcus_y,
                    y_blocks_per_mcu,
                )?
                // bit dropped here, returning &mut reader
            };

            match marker {
                M_EOI => break,
                M_SOS => continue,
                M_DHT => {
                    // some encoders interleave Huffman tables between
                    // scans; parse and continue scanning.
                    parse_dht(&mut reader, &mut st)?;
                    // next we expect another marker — keep looking
                    // for FF, then a non-fill byte.
                    let m = next_marker(&mut reader)?;
                    if m == M_SOS {
                        continue;
                    }
                    if m == M_EOI {
                        break;
                    }
                    log::warn!("jpeg: unexpected marker after DHT: {:#x}", m);
                    return Err("jpeg: unexpected marker between scans");
                }
                M_DQT | M_DRI => {
                    // table updates between scans
                    if marker == M_DQT {
                        parse_dqt(&mut reader, &mut st)?;
                    } else {
                        parse_dri(&mut reader, &mut st)?;
                    }
                    let m = next_marker(&mut reader)?;
                    if m == M_SOS {
                        continue;
                    }
                    if m == M_EOI {
                        break;
                    }
                    log::warn!("jpeg: unexpected marker after table update: {:#x}", m);
                    return Err("jpeg: unexpected marker between scans");
                }
                0xC0..=0xCF => {
                    log::warn!("jpeg: SOF mid-stream: {:#x}", marker);
                    return Err("jpeg: unexpected SOF mid-stream");
                }
                _ => {
                    // app/comment/DNL: skip the segment, then look for
                    // the next real marker.
                    skip_unknown_marker(&mut reader)?;
                    let m = next_marker(&mut reader)?;
                    if m == M_SOS {
                        continue;
                    }
                    if m == M_EOI {
                        break;
                    }
                    log::warn!("jpeg: unexpected marker after segment: {:#x}", m);
                    return Err("jpeg: unexpected marker between scans");
                }
            }
        }

        // dequantize + IDCT each Y block in the band, folding into the
        // binned MCU-row buffer
        let qt = &st.qt[st.comp[0].qt_idx as usize];
        let band_rows = band_bottom - band_top;
        for my in 0..band_rows {
            // one MCU row at a time: the band accumulates, so it has to
            // start from zero for each (the old full-width buffer was
            // overwritten wholesale and only needed filling once)
            band.clear();
            for mx in 0..mcus_x {
                for bv in 0..st.max_v as usize {
                    for bh in 0..st.max_h as usize {
                        let bidx = bv * st.max_h as usize + bh;
                        let off = band_block_offset(
                            my,
                            mx,
                            bidx,
                            mcus_x,
                            y_blocks_per_mcu,
                        );
                        for kk in 0..64 {
                            block_i32[kk] = (band_y[off + kk] as i32) * (qt[kk] as i32);
                        }
                        idct(&block_i32, &mut pix);
                        band.scatter(mx * mcu_w + bh * 8, bv * 8, w, &pix);
                    }
                }
            }

            // fold this MCU row into the output grid
            for py in 0..mcu_h {
                let src_y = (band_top + my) * mcu_h + py;
                if src_y >= h || src_y / scale >= out_h {
                    break;
                }
                band.fold_row(py, &mut acc_row);
                rows_in_acc += 1;
                if rows_in_acc as usize == scale {
                    emit_row(
                        &acc_row,
                        &band,
                        rows_in_acc,
                        &mut err_cur,
                        &mut err_nxt,
                        &mut output[out_y * out_stride..(out_y + 1) * out_stride],
                    );
                    out_y += 1;
                    core::mem::swap(&mut err_cur, &mut err_nxt);
                    err_nxt.fill(0);
                    acc_row.fill(0);
                    rows_in_acc = 0;
                }
            }
        }
    }

    if rows_in_acc > 0 && out_y < out_h {
        emit_row(
            &acc_row,
            &band,
            rows_in_acc,
            &mut err_cur,
            &mut err_nxt,
            &mut output[out_y * out_stride..(out_y + 1) * out_stride],
        );
        out_y += 1;
    }

    Ok(DecodedImage {
        width: out_w as u16,
        height: out_y as u16,
        data: output,
        stride: out_stride,
    })
}

// streaming marker parsing — reads byte-by-byte from any JpegRead

#[inline]
fn read_u16_be<R: JpegRead>(r: &mut R) -> Result<u16, &'static str> {
    let hi = r.read_byte()? as u16;
    let lo = r.read_byte()? as u16;
    Ok((hi << 8) | lo)
}

fn skip_bytes<R: JpegRead>(r: &mut R, n: usize) -> Result<(), &'static str> {
    for _ in 0..n {
        r.read_byte()?;
    }
    Ok(())
}

// walk to the next 0xFF, skip fill 0xFFs, return the marker code.
fn next_marker<R: JpegRead>(r: &mut R) -> Result<u8, &'static str> {
    loop {
        if r.read_byte()? == 0xFF {
            break;
        }
    }
    loop {
        let b = r.read_byte()?;
        if b != 0xFF {
            return Ok(b);
        }
    }
}

fn skip_unknown_marker<R: JpegRead>(r: &mut R) -> Result<(), &'static str> {
    let seg = read_u16_be(r)? as usize;
    if seg < 2 {
        return Err("jpeg: bad marker length");
    }
    skip_bytes(r, seg - 2)
}

fn parse_markers<R: JpegRead>(r: &mut R) -> Result<Box<JpegState>, &'static str> {
    // explicit signature check; map read errors to "invalid signature"
    // so a truncated input reports the same error as a wrong magic.
    let b1 = r.read_byte().map_err(|_| "jpeg: invalid signature")?;
    let b2 = r.read_byte().map_err(|_| "jpeg: invalid signature")?;
    if b1 != 0xFF || b2 != M_SOI {
        return Err("jpeg: invalid signature");
    }
    let mut st = JpegState::heap_new()?;
    loop {
        let marker = next_marker(r)?;
        match marker {
            0x00 | M_RST0..=M_RST7 => continue,
            M_SOF0 => parse_sof(r, &mut st, false)?,
            M_SOF2 => parse_sof(r, &mut st, true)?,
            // unsupported SOF variants: 1, 3, 5..=11, 13..=15
            0xC1 | 0xC3 | 0xC5..=0xCB | 0xCD..=0xCF => {
                return Err("jpeg: unsupported SOF variant");
            }
            M_DHT => parse_dht(r, &mut st)?,
            M_DQT => parse_dqt(r, &mut st)?,
            M_DRI => parse_dri(r, &mut st)?,
            M_SOS => {
                // record offset of the SOS segment header (right after
                // the FF DA marker bytes) so the progressive decoder
                // can seek back here for each band pass.
                st.first_sos_offset = r.position();
                parse_sos(r, &mut st)?;
                return Ok(st);
            }
            M_EOI => return Err("jpeg: EOI before SOS"),
            _ => skip_unknown_marker(r)?,
        }
    }
}

fn parse_sof<R: JpegRead>(
    r: &mut R,
    st: &mut JpegState,
    progressive: bool,
) -> Result<(), &'static str> {
    let seg = read_u16_be(r)? as usize;
    if seg < 2 + 6 {
        return Err("jpeg: SOF truncated");
    }
    let inner = seg - 2;

    let prec = r.read_byte()?;
    if prec != 8 {
        return Err("jpeg: only 8-bit precision");
    }
    st.height = read_u16_be(r)?;
    st.width = read_u16_be(r)?;
    let nc = r.read_byte()?;
    st.num_comp = nc;
    st.progressive = progressive;
    if nc == 0 || nc as usize > MAX_COMP {
        return Err("jpeg: bad component count");
    }
    let need = 6 + nc as usize * 3;
    if inner < need {
        return Err("jpeg: SOF truncated");
    }

    st.max_h = 1;
    st.max_v = 1;
    for i in 0..nc as usize {
        st.comp[i].id = r.read_byte()?;
        let samp = r.read_byte()?;
        st.comp[i].h_samp = samp >> 4;
        st.comp[i].v_samp = samp & 0x0F;
        st.comp[i].qt_idx = r.read_byte()?;
        if st.comp[i].h_samp == 0 || st.comp[i].v_samp == 0 {
            return Err("jpeg: zero sampling factor");
        }
        st.max_h = st.max_h.max(st.comp[i].h_samp);
        st.max_v = st.max_v.max(st.comp[i].v_samp);
    }
    skip_bytes(r, inner - need)
}

fn parse_dqt<R: JpegRead>(r: &mut R, st: &mut JpegState) -> Result<(), &'static str> {
    let seg = read_u16_be(r)? as usize;
    if seg < 2 {
        return Err("jpeg: DQT truncated");
    }
    let mut left = seg - 2;
    while left > 0 {
        if left < 1 {
            return Err("jpeg: DQT truncated");
        }
        let info = r.read_byte()?;
        left -= 1;
        let prec = info >> 4;
        let id = (info & 0x0F) as usize;
        if id >= 4 {
            return Err("jpeg: DQT id out of range");
        }
        if prec == 0 {
            if left < 64 {
                return Err("jpeg: DQT truncated");
            }
            for i in 0..64 {
                st.qt[id][i] = r.read_byte()? as u16;
            }
            left -= 64;
        } else {
            if left < 128 {
                return Err("jpeg: DQT truncated");
            }
            for i in 0..64 {
                st.qt[id][i] = read_u16_be(r)?;
            }
            left -= 128;
        }
        st.qt_ok[id] = true;
    }
    Ok(())
}

fn parse_dht<R: JpegRead>(r: &mut R, st: &mut JpegState) -> Result<(), &'static str> {
    let seg = read_u16_be(r)? as usize;
    if seg < 2 {
        return Err("jpeg: DHT truncated");
    }
    let mut left = seg - 2;
    while left > 0 {
        if left < 17 {
            return Err("jpeg: DHT truncated");
        }
        let info = r.read_byte()?;
        let class = info >> 4;
        let id = (info & 0x0F) as usize;
        if id >= 4 {
            return Err("jpeg: DHT id out of range");
        }
        let mut bits = [0u8; 16];
        for b in bits.iter_mut() {
            *b = r.read_byte()?;
        }
        let total: usize = bits.iter().map(|&b| b as usize).sum();
        if total > 256 || left < 17 + total {
            return Err("jpeg: DHT value overflow");
        }
        let mut vals = [0u8; 256];
        for v in vals.iter_mut().take(total) {
            *v = r.read_byte()?;
        }
        if class == 0 {
            build_huff_table(&mut st.dc_huff[id], &bits, &vals[..total]);
            st.dc_ok[id] = true;
        } else {
            build_huff_table(&mut st.ac_huff[id], &bits, &vals[..total]);
            st.ac_ok[id] = true;
        }
        left -= 17 + total;
    }
    Ok(())
}

fn parse_dri<R: JpegRead>(r: &mut R, st: &mut JpegState) -> Result<(), &'static str> {
    let seg = read_u16_be(r)? as usize;
    if seg < 4 {
        return Err("jpeg: DRI truncated");
    }
    st.restart_interval = read_u16_be(r)?;
    skip_bytes(r, seg - 4)
}

fn parse_sos<R: JpegRead>(r: &mut R, st: &mut JpegState) -> Result<(), &'static str> {
    let seg = read_u16_be(r)? as usize;
    if seg < 2 + 4 {
        return Err("jpeg: SOS truncated");
    }
    let inner = seg - 2;

    let nc = r.read_byte()?;
    st.scan_num_comp = nc;
    if nc == 0 || nc > st.num_comp {
        return Err("jpeg: bad SOS component count");
    }
    let need = 1 + nc as usize * 2 + 3;
    if inner < need {
        return Err("jpeg: SOS truncated");
    }

    for sci in 0..nc as usize {
        let cs = r.read_byte()?;
        let td_ta = r.read_byte()?;
        let mut found = false;
        for j in 0..st.num_comp as usize {
            if st.comp[j].id == cs {
                st.comp[j].dc_tbl = td_ta >> 4;
                st.comp[j].ac_tbl = td_ta & 0x0F;
                st.scan_order[sci] = j as u8;
                found = true;
                break;
            }
        }
        if !found {
            return Err("jpeg: SOS references unknown component");
        }
    }
    st.scan_ss = r.read_byte()?.min(63);
    st.scan_se = r.read_byte()?.min(63);
    let ah_al = r.read_byte()?;
    st.scan_ah = ah_al >> 4;
    st.scan_al = ah_al & 0x0F;

    skip_bytes(r, inner - need)
}

// finds the first SOF0/SOF2 marker and returns (width, height) without
// building full decoder state.
fn scan_sof_streaming<R: JpegRead>(r: &mut R) -> Result<(u16, u16), &'static str> {
    let b1 = r.read_byte().map_err(|_| "jpeg: invalid signature")?;
    let b2 = r.read_byte().map_err(|_| "jpeg: invalid signature")?;
    if b1 != 0xFF || b2 != M_SOI {
        return Err("jpeg: invalid signature");
    }
    loop {
        let marker = next_marker(r)?;
        match marker {
            0x00 | M_RST0..=M_RST7 => continue,
            M_SOF0 | M_SOF2 => {
                let seg = read_u16_be(r)? as usize;
                if seg < 2 + 5 {
                    return Err("jpeg: SOF truncated");
                }
                let prec = r.read_byte()?;
                if prec != 8 {
                    return Err("jpeg: only 8-bit precision");
                }
                let height = read_u16_be(r)?;
                let width = read_u16_be(r)?;
                if width == 0 || height == 0 {
                    return Err("jpeg: zero dimensions");
                }
                return Ok((width, height));
            }
            0xC1 | 0xC3 | 0xC5..=0xCB | 0xCD..=0xCF => {
                return Err("jpeg: unsupported SOF variant");
            }
            M_SOS => return Err("jpeg: SOS before SOF"),
            M_EOI => return Err("jpeg: EOI before SOF"),
            _ => skip_unknown_marker(r)?,
        }
    }
}

// Huffman table construction

fn build_huff_table(table: &mut HuffTable, bits: &[u8; 16], vals: &[u8]) {
    let total: usize = bits.iter().map(|&b| b as usize).sum();
    table.values[..total].copy_from_slice(&vals[..total]);
    table.lut.fill((0, 0));
    table.maxcode.fill(-1);

    let mut code: u32 = 0;
    let mut si: usize = 0;

    for bl in 1..=16usize {
        let cnt = bits[bl - 1] as usize;
        if cnt > 0 {
            table.valptr[bl] = si;
            table.mincode[bl] = code as i32;
            for _ in 0..cnt {
                if bl <= 8 {
                    let prefix = (code << (8 - bl)) as usize;
                    let fill = 1usize << (8 - bl);
                    for k in 0..fill {
                        if prefix + k < 256 {
                            table.lut[prefix + k] = (vals[si], bl as u8);
                        }
                    }
                }
                si += 1;
                code += 1;
            }
            table.maxcode[bl] = (code - 1) as i32;
        }
        code <<= 1;
    }
}

// Huffman decode

fn huff_decode<R: JpegRead>(r: &mut BitReader<R>, t: &HuffTable) -> Result<u8, &'static str> {
    let peek8 = r.peek(8)? as usize;
    let (sym, nb) = t.lut[peek8];
    if nb > 0 {
        r.drop_bits(nb);
        return Ok(sym);
    }
    let peek16 = r.peek(16)? as i32;
    for bl in 9..=16u8 {
        let code = peek16 >> (16 - bl);
        if t.maxcode[bl as usize] >= 0 && code <= t.maxcode[bl as usize] {
            r.drop_bits(bl);
            let idx = t.valptr[bl as usize] as i32 + code - t.mincode[bl as usize];
            return Ok(t.values[idx as usize]);
        }
    }
    Err("jpeg: invalid Huffman code")
}

#[inline]
fn extend(bits: u32, size: u8) -> i32 {
    let half = 1u32 << (size as u32 - 1);
    if bits < half {
        bits as i32 - ((1u32 << size as u32) as i32 - 1)
    } else {
        bits as i32
    }
}

// block decode (Y) / skip (non-Y)

fn decode_block<R: JpegRead>(
    r: &mut BitReader<R>,
    dc_ht: &HuffTable,
    ac_ht: &HuffTable,
    dc_pred: &mut i32,
    qt: &[u16; 64],
    blk: &mut [i32; 64],
    se: usize,
    al: u8,
) -> Result<(), &'static str> {
    blk.fill(0);

    let dc_size = huff_decode(r, dc_ht)?;
    if dc_size > 0 {
        if dc_size > 11 {
            return Err("jpeg: DC size > 11");
        }
        let bits = r.read_bits(dc_size)?;
        *dc_pred += extend(bits, dc_size);
    }
    blk[0] = ((*dc_pred) << al).wrapping_mul(qt[0] as i32);

    if se > 0 {
        let mut k: usize = 1;
        while k <= se {
            let sym = huff_decode(r, ac_ht)?;
            let run = (sym >> 4) as usize;
            let size = sym & 0x0F;
            if size == 0 {
                if run == 15 {
                    k += 16;
                } else {
                    break;
                }
            } else {
                k += run;
                if k > se {
                    return Err("jpeg: AC index overflow");
                }
                let bits = r.read_bits(size)?;
                let val = extend(bits, size);
                blk[ZZ[k]] = (val << al).wrapping_mul(qt[k] as i32);
                k += 1;
            }
        }
    }
    Ok(())
}

fn skip_block<R: JpegRead>(
    r: &mut BitReader<R>,
    dc_ht: &HuffTable,
    ac_ht: &HuffTable,
    dc_pred: &mut i32,
    se: usize,
) -> Result<(), &'static str> {
    let dc_size = huff_decode(r, dc_ht)?;
    if dc_size > 0 {
        if dc_size > 11 {
            return Err("jpeg: DC size > 11");
        }
        let bits = r.read_bits(dc_size)?;
        *dc_pred += extend(bits, dc_size);
    }
    if se > 0 {
        let mut k: usize = 1;
        while k <= se {
            let sym = huff_decode(r, ac_ht)?;
            let run = (sym >> 4) as usize;
            let size = sym & 0x0F;
            if size == 0 {
                if run == 15 {
                    k += 16;
                } else {
                    break;
                }
            } else {
                k += run + 1;
                let _ = r.read_bits(size)?;
            }
        }
    }
    Ok(())
}

// integer IDCT (IJG ISLOW, two-pass row + col)

fn idct(block: &[i32; 64], out: &mut [u8; 64]) {
    let mut ws = [0i32; 64];

    for row in 0..8 {
        let b = row * 8;
        let (d0, d1, d2, d3) = (block[b], block[b + 1], block[b + 2], block[b + 3]);
        let (d4, d5, d6, d7) = (block[b + 4], block[b + 5], block[b + 6], block[b + 7]);

        if d1 == 0 && d2 == 0 && d3 == 0 && d4 == 0 && d5 == 0 && d6 == 0 && d7 == 0 {
            let dc = d0 << P1;
            ws[b..b + 8].fill(dc);
            continue;
        }

        let z1 = (d2 + d6).wrapping_mul(F0541);
        let tmp2 = z1 + d6.wrapping_mul(-F1847);
        let tmp3 = z1 + d2.wrapping_mul(F0765);
        let tmp0 = (d0 + d4) << CB;
        let tmp1 = (d0 - d4) << CB;
        let (t10, t13) = (tmp0 + tmp3, tmp0 - tmp3);
        let (t11, t12) = (tmp1 + tmp2, tmp1 - tmp2);

        let (zz1, zz2, zz3, zz4) = (d7 + d1, d5 + d3, d7 + d3, d5 + d1);
        let z5 = (zz3 + zz4).wrapping_mul(F1175);
        let mut o0 = d7.wrapping_mul(F0298);
        let mut o1 = d5.wrapping_mul(F2053);
        let mut o2 = d3.wrapping_mul(F3072);
        let mut o3 = d1.wrapping_mul(F1501);
        let (s1, s2) = (zz1.wrapping_mul(-F0899), zz2.wrapping_mul(-F2562));
        let s3 = zz3.wrapping_mul(-F1961) + z5;
        let s4 = zz4.wrapping_mul(-F0390) + z5;
        o0 += s1 + s3;
        o1 += s2 + s4;
        o2 += s2 + s3;
        o3 += s1 + s4;

        let sh = CB - P1;
        ws[b] = descale(t10 + o3, sh);
        ws[b + 7] = descale(t10 - o3, sh);
        ws[b + 1] = descale(t11 + o2, sh);
        ws[b + 6] = descale(t11 - o2, sh);
        ws[b + 2] = descale(t12 + o1, sh);
        ws[b + 5] = descale(t12 - o1, sh);
        ws[b + 3] = descale(t13 + o0, sh);
        ws[b + 4] = descale(t13 - o0, sh);
    }

    for col in 0..8 {
        let (d0, d1, d2, d3) = (ws[col], ws[col + 8], ws[col + 16], ws[col + 24]);
        let (d4, d5, d6, d7) = (ws[col + 32], ws[col + 40], ws[col + 48], ws[col + 56]);

        if d1 == 0 && d2 == 0 && d3 == 0 && d4 == 0 && d5 == 0 && d6 == 0 && d7 == 0 {
            let v = clamp(descale(d0, P1 + 3) + 128);
            out[col] = v;
            out[col + 8] = v;
            out[col + 16] = v;
            out[col + 24] = v;
            out[col + 32] = v;
            out[col + 40] = v;
            out[col + 48] = v;
            out[col + 56] = v;
            continue;
        }

        let z1 = (d2 + d6).wrapping_mul(F0541);
        let tmp2 = z1 + d6.wrapping_mul(-F1847);
        let tmp3 = z1 + d2.wrapping_mul(F0765);
        let tmp0 = (d0 + d4) << CB;
        let tmp1 = (d0 - d4) << CB;
        let (t10, t13) = (tmp0 + tmp3, tmp0 - tmp3);
        let (t11, t12) = (tmp1 + tmp2, tmp1 - tmp2);

        let (zz1, zz2, zz3, zz4) = (d7 + d1, d5 + d3, d7 + d3, d5 + d1);
        let z5 = (zz3 + zz4).wrapping_mul(F1175);
        let mut o0 = d7.wrapping_mul(F0298);
        let mut o1 = d5.wrapping_mul(F2053);
        let mut o2 = d3.wrapping_mul(F3072);
        let mut o3 = d1.wrapping_mul(F1501);
        let (s1, s2) = (zz1.wrapping_mul(-F0899), zz2.wrapping_mul(-F2562));
        let s3 = zz3.wrapping_mul(-F1961) + z5;
        let s4 = zz4.wrapping_mul(-F0390) + z5;
        o0 += s1 + s3;
        o1 += s2 + s4;
        o2 += s2 + s3;
        o3 += s1 + s4;

        let sh = CB + P1 + 3;
        out[col] = clamp(descale(t10 + o3, sh) + 128);
        out[col + 56] = clamp(descale(t10 - o3, sh) + 128);
        out[col + 8] = clamp(descale(t11 + o2, sh) + 128);
        out[col + 48] = clamp(descale(t11 - o2, sh) + 128);
        out[col + 16] = clamp(descale(t12 + o1, sh) + 128);
        out[col + 40] = clamp(descale(t12 - o1, sh) + 128);
        out[col + 24] = clamp(descale(t13 + o0, sh) + 128);
        out[col + 32] = clamp(descale(t13 - o0, sh) + 128);
    }
}

// Floyd-Steinberg dithering

// average one output row out of the accumulator and dither it inline.
// `acc[ox]` holds the sum of every source pixel in the box that maps to
// output column `ox`, over `n_row` source rows; dividing by the box's
// sample count is the box filter.
#[inline]
fn emit_row(
    acc: &[u32],
    band: &Band,
    n_row: u32,
    err_cur: &mut [i16],
    err_nxt: &mut [i16],
    out_row: &mut [u8],
) {
    for ox in 0..band.out_w {
        let n = band.col_n[ox] as u32 * n_row;
        // a bin no block covered keeps the mid-grey the full-width
        // band used to supply via fill(128)
        let g = acc[ox]
            .checked_div(n)
            .map_or(128i16, |avg| ((avg << band.shift).min(255)) as i16);
        let val = (g + err_cur[ox + 1]).clamp(0, 255);
        let black = val < 128;
        let q = if black { 0i16 } else { 255 };
        let e = val - q;
        if black {
            out_row[ox / 8] |= 1 << (7 - (ox & 7));
        }
        err_cur[ox + 2] += e * 7 / 16;
        err_nxt[ox] += e * 3 / 16;
        err_nxt[ox + 1] += e * 5 / 16;
        err_nxt[ox + 2] += e / 16;
    }
}

// helpers

#[inline]
fn descale(x: i32, n: i32) -> i32 {
    (x + (1 << (n - 1))) >> n
}

#[inline]
fn clamp(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}
