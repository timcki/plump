// declarative byte-record codec
//
// on-disk records used to be hand-written decode/encode pairs against
// manual offset constants. the two halves drifted, so every record now
// declares its fields ONCE via `record!` and the macro generates the
// struct, `decode`, `encode`, `SIZE`, and a compile-time proof that no
// two fields overlap and none runs past the end of the record.
//
// see `record!` for the field grammar.

use crate::util::FixedStr;

/// A value that occupies a fixed run of bytes inside a record.
///
/// `read` receives a slice starting at the field's offset and must not
/// look past `WIDTH` bytes; it returns `None` when the bytes do not
/// form a valid value (unknown enum discriminant, short buffer).
pub trait Field: Sized + Copy + PartialEq {
    /// bytes this field occupies on disk
    const WIDTH: usize;
    /// value used to pre-fill array fields before decoding
    const ZERO: Self;

    fn read(src: &[u8]) -> Option<Self>;
    fn write(self, dst: &mut [u8]);
}

impl Field for u8 {
    const WIDTH: usize = 1;
    const ZERO: Self = 0;

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        src.first().copied()
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        dst[0] = self;
    }
}

impl Field for u16 {
    const WIDTH: usize = 2;
    const ZERO: Self = 0;

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        let b: [u8; 2] = src.get(..2)?.try_into().ok()?;
        Some(u16::from_le_bytes(b))
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        dst[..2].copy_from_slice(&self.to_le_bytes());
    }
}

impl Field for u32 {
    const WIDTH: usize = 4;
    const ZERO: Self = 0;

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        let b: [u8; 4] = src.get(..4)?.try_into().ok()?;
        Some(u32::from_le_bytes(b))
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        dst[..4].copy_from_slice(&self.to_le_bytes());
    }
}

impl<const N: usize> Field for [u8; N] {
    const WIDTH: usize = N;
    const ZERO: Self = [0u8; N];

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        src.get(..N)?.try_into().ok()
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        dst[..N].copy_from_slice(&self);
    }
}

/// Read a length-prefixed fixed string: one length byte at `len_off`,
/// `N` body bytes at `body_off`. The stored length is clamped to `N`,
/// matching the pre-macro decoders.
#[inline]
pub fn read_fixed_str<const N: usize>(
    buf: &[u8],
    len_off: usize,
    body_off: usize,
) -> Option<FixedStr<N>> {
    let n = (*buf.get(len_off)? as usize).min(N);
    let mut b = [0u8; N];
    b[..n].copy_from_slice(buf.get(body_off..body_off + n)?);
    Some(FixedStr::from_raw(b, n as u8))
}

/// Write a length-prefixed fixed string. Bytes past the string length
/// keep whatever the buffer already held (zero, for a fresh record).
#[inline]
pub fn write_fixed_str<const N: usize>(
    buf: &mut [u8],
    len_off: usize,
    body_off: usize,
    s: &FixedStr<N>,
) {
    let n = s.len().min(N);
    buf[len_off] = n as u8;
    buf[body_off..body_off + n].copy_from_slice(&s.raw_buf()[..n]);
}

/// Claim `width` bytes at `off` in a record coverage map, tripping a
/// compile error when the span leaves the record or collides with a
/// field already claimed.
#[doc(hidden)]
pub const fn mark_span(used: &mut [bool], off: usize, width: usize) {
    let mut i = 0;
    while i < width {
        assert!(off + i < used.len(), "record field runs past the record");
        assert!(!used[off + i], "record fields overlap");
        used[off + i] = true;
        i += 1;
    }
}

// ── macro internals ────────────────────────────────────────────────
//
// each field is written `name: KIND @ OFFSET`. KIND and OFFSET are
// single token trees so the field list can be expanded three times
// (struct, decode, encode) plus once for the coverage proof, without a
// token muncher.

#[doc(hidden)]
#[macro_export]
macro_rules! __record_ty {
    (bool16) => { bool };
    ({str $cap:expr}) => { $crate::util::FixedStr<$cap> };
    ({arr $t:ty; $n:expr}) => { [$t; $n] };
    ({$t:ty}) => { $t };
    ($t:ty) => { $t };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __record_read {
    (bool16, $off:tt, $buf:expr) => {
        (<u16 as $crate::util::Field>::read(&$buf[$off..])? & 1) != 0
    };
    ({str $cap:expr}, ($lo:expr, $bo:expr), $buf:expr) => {
        $crate::util::read_fixed_str::<$cap>($buf, $lo, $bo)?
    };
    ({arr $t:ty; $n:expr}, $tbl:tt, $buf:expr) => {{
        let mut out = [<$t as $crate::util::Field>::ZERO; $n];
        let mut i = 0usize;
        while i < $n {
            out[i] = <$t as $crate::util::Field>::read(&$buf[$tbl[i]..])?;
            i += 1;
        }
        out
    }};
    ({$t:ty}, $off:tt, $buf:expr) => {
        <$t as $crate::util::Field>::read(&$buf[$off..])?
    };
    ($t:ty, $off:tt, $buf:expr) => {
        <$t as $crate::util::Field>::read(&$buf[$off..])?
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __record_write {
    (bool16, $off:tt, $buf:expr, $val:expr) => {
        <u16 as $crate::util::Field>::write(
            if $val { 1u16 } else { 0u16 },
            &mut $buf[$off..],
        )
    };
    ({str $cap:expr}, ($lo:expr, $bo:expr), $buf:expr, $val:expr) => {
        $crate::util::write_fixed_str::<$cap>(&mut $buf, $lo, $bo, &$val)
    };
    ({arr $t:ty; $n:expr}, $tbl:tt, $buf:expr, $val:expr) => {{
        let mut i = 0usize;
        while i < $n {
            <$t as $crate::util::Field>::write($val[i], &mut $buf[$tbl[i]..]);
            i += 1;
        }
    }};
    ({$t:ty}, $off:tt, $buf:expr, $val:expr) => {
        <$t as $crate::util::Field>::write($val, &mut $buf[$off..])
    };
    ($t:ty, $off:tt, $buf:expr, $val:expr) => {
        <$t as $crate::util::Field>::write($val, &mut $buf[$off..])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __record_mark {
    (bool16, $off:tt, $used:expr) => {
        $crate::util::mark_span(&mut $used, $off, 2)
    };
    ({str $cap:expr}, ($lo:expr, $bo:expr), $used:expr) => {{
        $crate::util::mark_span(&mut $used, $lo, 1);
        $crate::util::mark_span(&mut $used, $bo, $cap);
    }};
    ({arr $t:ty; $n:expr}, $tbl:tt, $used:expr) => {{
        let mut i = 0usize;
        while i < $n {
            $crate::util::mark_span(
                &mut $used,
                $tbl[i],
                <$t as $crate::util::Field>::WIDTH,
            );
            i += 1;
        }
    }};
    ({$t:ty}, $off:tt, $used:expr) => {
        $crate::util::mark_span(
            &mut $used,
            $off,
            <$t as $crate::util::Field>::WIDTH,
        )
    };
    ($t:ty, $off:tt, $used:expr) => {
        $crate::util::mark_span(
            &mut $used,
            $off,
            <$t as $crate::util::Field>::WIDTH,
        )
    };
}

/// Declare an on-disk record from a single field list.
///
/// ```ignore
/// record! {
///     /// twelve bytes describing one page
///     #[derive(Clone, Copy, Debug, PartialEq, Eq)]
///     pub struct PageRecord [PAGE_RECORD_SIZE] {
///         first_line: u16 @ 0,
///         line_count: u8  @ 2,
///         // 3 is a pad byte: undeclared bytes stay zero on encode
///         start_byte: u32 @ 4,
///     }
/// }
/// ```
///
/// Field kinds:
/// * any [`Field`] type (`u8` / `u16` / `u32` / `[u8; N]` / a custom
///   impl) at a byte offset;
/// * `bool16`: a `u16` whose bit 0 is the flag; encodes as 0 or 1;
/// * `{str CAP}` at `(len_off, body_off)`: a [`FixedStr`] stored as a
///   length byte plus a fixed body;
/// * `{arr T; N}` at a `[usize; N]` const table: N `Field` values at
///   listed offsets, so an index-shaped section table stays an array;
/// * `{T}` for any `Field` type that is more than one token, e.g.
///   `{Option<DayKey>}`.
///
/// Optional trailing blocks:
/// * `fixed { name: KIND @ OFF = VALUE, .. }` — bytes that are written
///   on encode and must match on decode (magic, format version), and
///   which are not struct fields;
/// * `extra { name: TY = DEFAULT, .. }` — struct fields that are not on
///   disk; decode fills them with DEFAULT;
/// * `verify EXPR;` — a `fn(&Self) -> bool` run after decode; a false
///   result turns the decode into `None`.
///
/// Generates `SIZE`, `decode(&[u8]) -> Option<Self>`,
/// `encode(&self) -> [u8; SIZE]`, and a const block proving the fields
/// are disjoint and in bounds.
#[macro_export]
macro_rules! record {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident [$size:expr] {
            $( $(#[$fmeta:meta])* $fname:ident : $fkind:tt @ $foff:tt ),* $(,)?
        }
        $( fixed {
            $( $cname:ident : $ckind:tt @ $coff:tt = $cval:expr ),* $(,)?
        } )?
        $( extra {
            $( $xname:ident : $xty:ty = $xdef:expr ),* $(,)?
        } )?
        $( verify $vf:expr ; )?
    ) => {
        $(#[$meta])*
        $vis struct $Name {
            $( $(#[$fmeta])* pub $fname : $crate::__record_ty!($fkind), )*
            $( $( $xname : $xty, )* )?
        }

        impl $Name {
            /// bytes this record occupies on disk
            pub const SIZE: usize = $size;

            /// Decode one record. `None` when the buffer is short, a
            /// fixed field does not match, or a field rejects its bytes.
            pub fn decode(buf: &[u8]) -> Option<Self> {
                if buf.len() < $size {
                    return None;
                }
                $($(
                    if $crate::__record_read!($ckind, $coff, buf) != $cval {
                        return None;
                    }
                )*)?
                #[allow(clippy::needless_update)]
                let rec = Self {
                    $( $fname: $crate::__record_read!($fkind, $foff, buf), )*
                    $( $( $xname: $xdef, )* )?
                };
                $(
                    if !($vf)(&rec) {
                        return None;
                    }
                )?
                Some(rec)
            }

            /// Encode one record. Bytes not covered by a field stay
            /// zero, which is what the reserved gaps expect.
            pub fn encode(&self) -> [u8; $size] {
                let mut out = [0u8; $size];
                $($(
                    $crate::__record_write!($ckind, $coff, out, $cval);
                )*)?
                $(
                    $crate::__record_write!($fkind, $foff, out, self.$fname);
                )*
                out
            }
        }

        // proof that the declared fields tile the record without
        // overlapping and without running off the end
        const _: () = {
            let mut used = [false; $size];
            $($( $crate::__record_mark!($ckind, $coff, used); )*)?
            $( $crate::__record_mark!($fkind, $foff, used); )*
        };
    };
}
