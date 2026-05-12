/*
 * Copyright (c) 2026 Jonathan Perkin <jonathan@perkin.org.uk>
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

//! Typed views over a writer-owned page buffer.
//!
//! The btree page layout is a flat byte buffer whose first 20
//! bytes are the [`PAGE_HEADER_SIZE`]-byte header, followed by an
//! `indx_t` (`u16`) array `linp[]` growing upward from the header,
//! with entries placed at the offsets named in `linp[]` and
//! growing downward from the end of the page.  See `btree.h` for
//! the canonical layout.
//!
//! Most algorithmic code in the writer doesn't want to think in
//! terms of "byte 16 is `lower`, byte 18 is `upper`"; it wants
//! [`PageBufRef::lower`], [`PageBufRef::upper`],
//! [`PageBufRef::linp`], [`PageBufMut::reserve_slot`].  This
//! module is the seam where those raw byte offsets are translated
//! into named methods, so spot bugs of the form "I wrote `linp[i]`
//! but at the wrong offset" become structurally impossible.
//!
//! Two flavours:
//!
//! - [`PageBufRef<'a>`] is `Copy`, wraps a borrowed `&'a [u8]`,
//!   and exposes only read accessors.  Cheap to construct and
//!   pass around.
//! - [`PageBufMut<'a>`] wraps a borrowed `&'a mut [u8]` and adds
//!   the setters plus a small set of slot-manipulation operations
//!   ([`PageBufMut::reserve_slot`], [`PageBufMut::push_slot`])
//!   that keep `lower` / `upper` / `linp[]` in sync.
//!
//! There is intentionally no error handling here.  The writer
//! constructs every page it sees and trusts its own header values;
//! malformed pages would indicate a writer bug, not bad input.
//! Range checks happen via slice indexing (which panics) rather
//! than via fallible getters.  See the corresponding read-side
//! [`crate::page::PageView`] for the validated parse used on file
//! load.
//!
//! [`PAGE_HEADER_SIZE`]: crate::format::PAGE_HEADER_SIZE

use crate::format::{MAX_PSIZE, P_BLEAF, P_INVALID, PAGE_HEADER_SIZE, align_entry};

/// Read-only view over a writer-owned page buffer.
///
/// Wraps `&'a [u8]` and exposes named header fields plus `linp[]`
/// accessors.  See module-level documentation for the design.
#[derive(Clone, Copy)]
pub(crate) struct PageBufRef<'a> {
    bytes: &'a [u8],
}

impl<'a> PageBufRef<'a> {
    /// Wrap a borrowed page-sized byte slice.
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Page number of the previous sibling at this level (header
    /// offset 4); `P_INVALID` if this is the leftmost page.
    pub(crate) fn prev_pg(&self) -> u32 {
        read_u32(self.bytes, 4)
    }

    /// Page number of the next sibling at this level (header
    /// offset 8); `P_INVALID` if this is the rightmost page.
    pub(crate) fn next_pg(&self) -> u32 {
        read_u32(self.bytes, 8)
    }

    /// Page flags word (header offset 12).  Holds the page-type
    /// bits (`P_BLEAF`, `P_BINTERNAL`, `P_OVERFLOW`, ...) under
    /// the `P_TYPE` mask, plus orthogonal markers such as
    /// `P_PRESERVE`.  Note that `P_ROOT` is *not* a flag - it is
    /// the conventional page number of the root (always pgno 1).
    pub(crate) fn flags(&self) -> u32 {
        read_u32(self.bytes, 12)
    }

    /// `lower` (header offset 16) - end of `linp[]`, in bytes.
    /// Equivalent to `PAGE_HEADER_SIZE + nentries * 2`.
    pub(crate) fn lower(&self) -> usize {
        read_u16(self.bytes, 16) as usize
    }

    /// `upper` (header offset 18) - start of the entry payload
    /// region, in bytes.  Entries grow downward from this offset.
    pub(crate) fn upper(&self) -> usize {
        read_u16(self.bytes, 18) as usize
    }

    /// Number of entries currently in `linp[]`.
    pub(crate) fn nentries(&self) -> usize {
        (self.lower() - PAGE_HEADER_SIZE) / 2
    }

    /// Free space, in bytes, between the end of `linp[]` and the
    /// start of the entry payload region.  A new entry of `nbytes`
    /// aligned bytes (plus its 2-byte `linp[]` slot) fits iff
    /// `free_space() >= nbytes + 2`.
    pub(crate) fn free_space(&self) -> usize {
        self.upper() - self.lower()
    }

    /// `true` if the page is a btree leaf (`P_BLEAF`).
    pub(crate) fn is_leaf(&self) -> bool {
        self.flags() & P_BLEAF != 0
    }

    /// `true` if this is the rightmost page on its level
    /// (`next_pg == P_INVALID`).  Used by `bt_page`'s sorted-append
    /// shortcut.
    pub(crate) fn is_rightmost(&self) -> bool {
        self.next_pg() == P_INVALID
    }

    /// The `i`-th `linp[]` entry: a byte offset pointing into the
    /// payload region where entry `i` is stored.
    pub(crate) fn linp(&self, i: usize) -> u16 {
        read_u16(self.bytes, PAGE_HEADER_SIZE + i * 2)
    }

    /// Borrow the underlying byte slice for code that still needs
    /// raw access (e.g. handing the page to [`crate::page::PageView`]
    /// for full validated decoding, or to a delete/copy helper
    /// that does its own offset arithmetic).
    pub(crate) const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Mutable view over a writer-owned page buffer.  See module-level
/// documentation.
pub(crate) struct PageBufMut<'a> {
    bytes: &'a mut [u8],
}

impl<'a> PageBufMut<'a> {
    /// Wrap a borrowed page-sized mutable byte slice.
    pub(crate) const fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes }
    }

    /// Downgrade to an immutable view for read-only operations.
    pub(crate) const fn as_ref(&self) -> PageBufRef<'_> {
        PageBufRef::new(self.bytes)
    }

    // ----- header writes ----------------------------------------------

    /// Set this page's own page number (header offset 0).
    pub(crate) fn set_pgno(&mut self, v: u32) {
        write_u32(self.bytes, 0, v);
    }

    /// Set the previous-sibling page number (header offset 4).
    pub(crate) fn set_prev_pg(&mut self, v: u32) {
        write_u32(self.bytes, 4, v);
    }

    /// Set the next-sibling page number (header offset 8).
    pub(crate) fn set_next_pg(&mut self, v: u32) {
        write_u32(self.bytes, 8, v);
    }

    /// Set the page flags word (header offset 12).
    pub(crate) fn set_flags(&mut self, v: u32) {
        write_u32(self.bytes, 12, v);
    }

    /// Set `lower` (header offset 16); must equal
    /// `PAGE_HEADER_SIZE + nentries * 2` afterwards.
    ///
    /// # Panics
    ///
    /// Panics if `v > MAX_PSIZE`.  This is a writer-internal
    /// invariant; the check is unconditional (not debug-only) to
    /// stop a release build from silently wrapping a malformed
    /// `lower` value through `(indx_t)`-style truncation.
    pub(crate) fn set_lower(&mut self, v: usize) {
        assert!(v <= MAX_PSIZE, "lower {v} exceeds MAX_PSIZE {MAX_PSIZE}");
        write_u16(self.bytes, 16, page_offset_as_u16(v));
    }

    /// Set `upper` (header offset 18); must point at the smallest
    /// entry offset currently in use, or at `psize` when no entries
    /// are present.
    ///
    /// # Panics
    ///
    /// Panics if `v > MAX_PSIZE`.  See [`Self::set_lower`].
    pub(crate) fn set_upper(&mut self, v: usize) {
        assert!(v <= MAX_PSIZE, "upper {v} exceeds MAX_PSIZE {MAX_PSIZE}");
        write_u16(self.bytes, 18, page_offset_as_u16(v));
    }

    /// Set the `i`-th `linp[]` entry.
    ///
    /// `linp[i]` points at a real entry payload offset within
    /// `[upper, psize)`, so it is strictly less than `MAX_PSIZE`.
    ///
    /// # Panics
    ///
    /// Panics if `v >= MAX_PSIZE`.  See [`Self::set_lower`] for
    /// the rationale behind the unconditional check.
    pub(crate) fn set_linp(&mut self, i: usize, v: usize) {
        assert!(
            v < MAX_PSIZE,
            "linp[{i}] = {v} must be < MAX_PSIZE {MAX_PSIZE}"
        );
        write_u16(self.bytes, PAGE_HEADER_SIZE + i * 2, page_offset_as_u16(v));
    }

    // ----- delegated reads --------------------------------------------

    /// See [`PageBufRef::flags`].
    pub(crate) fn flags(&self) -> u32 {
        self.as_ref().flags()
    }
    /// See [`PageBufRef::lower`].
    pub(crate) fn lower(&self) -> usize {
        self.as_ref().lower()
    }
    /// See [`PageBufRef::upper`].
    pub(crate) fn upper(&self) -> usize {
        self.as_ref().upper()
    }
    /// See [`PageBufRef::nentries`].
    pub(crate) fn nentries(&self) -> usize {
        self.as_ref().nentries()
    }
    /// See [`PageBufRef::free_space`].
    pub(crate) fn free_space(&self) -> usize {
        self.as_ref().free_space()
    }
    /// See [`PageBufRef::linp`].
    pub(crate) fn linp(&self, i: usize) -> u16 {
        self.as_ref().linp(i)
    }

    // ----- raw access (preserved for entry-level byte work) -----------

    /// Borrow the underlying byte slice immutably (e.g. to hand to
    /// [`crate::page::PageView`]).
    pub(crate) const fn bytes(&self) -> &[u8] {
        self.bytes
    }

    /// Borrow the underlying byte slice mutably (e.g. to write an
    /// entry payload directly at a specific offset).
    pub(crate) const fn bytes_mut(&mut self) -> &mut [u8] {
        self.bytes
    }

    // ----- structured slot operations ---------------------------------

    /// Reserve `linp[idx]` for a new entry occupying `nbytes`
    /// aligned bytes.  Shifts `linp[idx..nentries]` up by one,
    /// increments `lower` by 2, decrements `upper` by `nbytes`,
    /// and writes `linp[idx] = new_upper`.  Returns the new
    /// `upper` so the caller can write the entry payload starting
    /// there.
    ///
    /// # Panics
    ///
    /// Panics if `free_space() < nbytes + 2`, if `nbytes` is not
    /// 4-byte aligned, or if `idx > nentries()`.  All three are
    /// writer-internal preconditions; checking them unconditionally
    /// (not just under `debug_assertions`) means a release-build
    /// bug bails out loudly rather than silently producing
    /// overlapping entries or a malformed linp shift.
    pub(crate) fn reserve_slot(&mut self, idx: usize, nbytes: usize) -> usize {
        let n = self.nentries();
        assert!(idx <= n, "reserve_slot idx {idx} past nentries {n}");
        assert_eq!(
            nbytes,
            align_entry(nbytes),
            "entry size {nbytes} not aligned"
        );
        assert!(
            self.free_space() >= nbytes + 2,
            "no room to reserve slot: free_space {free} < {needed}",
            free = self.free_space(),
            needed = nbytes + 2,
        );
        let lower = self.lower();
        let upper = self.upper();
        if idx < n {
            let start = PAGE_HEADER_SIZE + idx * 2;
            let end = PAGE_HEADER_SIZE + n * 2;
            self.bytes.copy_within(start..end, start + 2);
        }
        let new_upper = upper - nbytes;
        self.set_linp(idx, new_upper);
        self.set_lower(lower + 2);
        self.set_upper(new_upper);
        new_upper
    }
}

// ----- primitive byte reads / writes ----------------------------------

#[inline]
fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn write_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_ne_bytes());
}

#[inline]
fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

/// Encode a page byte offset (lower / upper / linp entry) as `u16`.
/// [`MAX_PSIZE`] is capped at 32768, so every legal offset fits in
/// `u16` directly with no sentinel wrap.
#[inline]
fn page_offset_as_u16(off: usize) -> u16 {
    debug_assert!(off <= MAX_PSIZE);
    u16::try_from(off).expect("page offset within MAX_PSIZE")
}
