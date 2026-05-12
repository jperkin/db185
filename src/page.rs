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

/*!
 * Decoding of btree pages.
 *
 * A page is a flat `psize`-byte buffer.  It begins with a fixed
 * [`PAGE_HEADER_SIZE`]-byte header, followed by an `indx_t` (u16) array
 * `linp[]` growing upward from the header, with entries stored at the
 * offsets named in `linp[]` growing downward from the end of the page.
 * See `btree.h` for the canonical layout.
 *
 * [`PageView::parse`] validates and decodes the header in one shot.
 * Once a `PageView` exists its header fields are immediately available
 * as plain values; per-entry decoders still return [`Result`] because
 * each entry's bounds are checked on access.
 *
 * Entry payload offsets and lengths are bounded by the page size
 * (`MAX_PSIZE = 32768`), so they fit comfortably in `u32`; the
 * cast-truncation allow inside [`PageView`]'s entry decoder is
 * justified by that invariant.
 */

use crate::Result;
use crate::error::Error;
use crate::format::{
    BINTERNAL_HEADER_SIZE, BLEAF_HEADER_SIZE, NOVFLSIZE, P_BIGDATA, P_BIGKEY, P_BINTERNAL, P_BLEAF,
    P_OVERFLOW, P_TYPE, PAGE_HEADER_SIZE,
};

/// Parsed, validated view over a single page's bytes.
///
/// Construct via [`PageView::parse`].  The header fields are decoded
/// up front so all queries about page type, links and entry count
/// are infallible.
pub(crate) struct PageView<'a> {
    pub(crate) pgno: u32,
    pub(crate) prev_pg: u32,
    pub(crate) next_pg: u32,
    bytes: &'a [u8],
    swap: bool,
    /// Page type and other bits; queried through [`PageView::is_leaf`]
    /// and friends rather than directly.
    flags: u32,
    /// `linp[]` slot count, precomputed from the page header's `lower`.
    nentries: usize,
}

/// Decoded leaf entry.  Carries no lifetime, so it outlives the
/// [`PageView`] it was decoded from; the owning page bytes are
/// supplied separately when the caller wants to materialise the data.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LeafEntry {
    pub(crate) key: ItemRef,
    pub(crate) data: ItemRef,
}

/// Decoded btree internal entry: a separator key plus the child page
/// number whose subtree holds keys `>=` the separator.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InternalEntry {
    pub(crate) key: ItemRef,
    pub(crate) child: u32,
}

/// A key or value: either an inline slice within the page, or a
/// reference to the first page of an overflow chain.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ItemRef {
    /// Inline bytes at `[off, off + len)` within the owning page.
    Inline { off: u32, len: u32 },
    /// `{pgno, size}` reference to an overflow chain of `size` bytes
    /// starting at page `pgno`.
    Overflow { pgno: u32, size: u32 },
}

impl<'a> PageView<'a> {
    /// Parse and validate `bytes` as page `pgno`.
    ///
    /// Decodes the header and checks the cheap structural
    /// invariants:
    ///
    /// - The slice is at least [`PAGE_HEADER_SIZE`] bytes long.
    /// - `lower` and `upper` form a valid `(linp-end, entry-start)`
    ///   pair within the page.
    /// - `flags & P_TYPE` is `P_BINTERNAL`, `P_BLEAF` or
    ///   `P_OVERFLOW`.
    ///
    /// `linp[i]` is *not* validated up front.  An offset that
    /// points outside the entry region (or one whose alignment is
    /// off) will be caught lazily when the entry is accessed: the
    /// `bytes.get(off..off + n)` calls inside [`PageView::u16_at`]
    /// / [`PageView::u32_at`] return `None`, which surfaces as a
    /// [`Error::CorruptPage`] at exactly the same end-user
    /// granularity but without paying for an `O(nentries)` loop on
    /// every page parse.  For pkgsrc workloads the source is a
    /// locally-written `pkgdb.byfile.db`, so the only realistic
    /// corruption mode is a torn write or disk bit-flip, which the
    /// lazy path catches the moment the affected entry is read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptPage`] if any of the header-level
    /// checks above fail.
    pub(crate) fn parse(pgno: u32, bytes: &'a [u8], swap: bool) -> Result<Self> {
        if bytes.len() < PAGE_HEADER_SIZE {
            return Err(Error::CorruptPage {
                pgno,
                reason: "page shorter than header",
            });
        }
        let prev_pg = read_u32(bytes, 4, swap);
        let next_pg = read_u32(bytes, 8, swap);
        let flags = read_u32(bytes, 12, swap);
        let lower = read_u16(bytes, 16, swap) as usize;
        let upper = read_u16(bytes, 18, swap) as usize;

        if lower < PAGE_HEADER_SIZE
            || upper > bytes.len()
            || lower > upper
            || (lower - PAGE_HEADER_SIZE) % 2 != 0
        {
            return Err(Error::CorruptPage {
                pgno,
                reason: "page bounds inconsistent",
            });
        }

        match flags & P_TYPE {
            P_BINTERNAL | P_BLEAF | P_OVERFLOW => {}
            _ => {
                return Err(Error::CorruptPage {
                    pgno,
                    reason: "unsupported page type",
                });
            }
        }

        Ok(Self {
            pgno,
            prev_pg,
            next_pg,
            flags,
            bytes,
            swap,
            nentries: (lower - PAGE_HEADER_SIZE) / 2,
        })
    }

    /// `true` if this is a btree leaf page.
    #[inline]
    #[must_use]
    pub(crate) const fn is_leaf(&self) -> bool {
        self.flags & P_BLEAF != 0
    }

    /// `true` if this is a btree internal page.
    #[inline]
    #[must_use]
    pub(crate) const fn is_internal(&self) -> bool {
        self.flags & P_BINTERNAL != 0
    }

    /// `true` if this is an overflow-chain page.
    #[inline]
    #[must_use]
    pub(crate) const fn is_overflow(&self) -> bool {
        self.flags & P_OVERFLOW != 0
    }

    /// Number of entries referenced by `linp[]`.
    #[inline]
    #[must_use]
    pub(crate) const fn nentries(&self) -> usize {
        self.nentries
    }

    /// Bytes of an overflow page's payload (everything after the
    /// page header).
    #[inline]
    #[must_use]
    pub(crate) const fn overflow_payload(&self) -> &'a [u8] {
        let (_, rest) = self.bytes.split_at(PAGE_HEADER_SIZE);
        rest
    }

    /// Resolve an [`ItemRef`] decoded from this page to its inline
    /// bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptPage`] if `item` is an overflow
    /// reference, or if its inline offset/length falls outside this
    /// page (which only happens if `item` came from a different page
    /// than `self`, since [`PageView::leaf_entry`] /
    /// [`PageView::internal_entry`] check bounds at decode time).
    pub(crate) fn inline(&self, item: ItemRef) -> Result<&'a [u8]> {
        match item {
            ItemRef::Inline { off, len } => {
                let start = off as usize;
                // `slice::get(Range)` handles overflow internally:
                // if `start + len` wraps it returns `None` rather
                // than panicking or aliasing.
                let end = start
                    .checked_add(len as usize)
                    .ok_or_else(|| self.corrupt("inline offset + length overflows"))?;
                self.bytes
                    .get(start..end)
                    .ok_or_else(|| self.corrupt("inline offset out of range"))
            }
            ItemRef::Overflow { .. } => Err(self.corrupt("expected inline item, found overflow")),
        }
    }

    /// Decode the leaf entry at `idx`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptPage`] if `idx` is out of range, if any
    /// of the entry's fields fall outside the page, or if an overflow
    /// reference is shorter than [`NOVFLSIZE`].
    pub(crate) fn leaf_entry(&self, idx: usize) -> Result<LeafEntry> {
        let off = self.entry_offset(idx)?;
        let ksize = self.u32_at(off)? as usize;
        let dsize = self.u32_at(off + 4)? as usize;
        let eflags = self.byte_at(off + 8)?;
        let payload = off + BLEAF_HEADER_SIZE;
        // Use checked arithmetic so a crafted (ksize, dsize) pair
        // can't wrap on a 32-bit target.  `bytes.len()` is bounded
        // by `MAX_PSIZE = 32768`, so on the happy path neither
        // sum nor compare can wrap; the check is defence in depth.
        let payload_end = payload
            .checked_add(ksize)
            .and_then(|p| p.checked_add(dsize))
            .ok_or_else(|| self.corrupt("leaf payload length overflows"))?;
        if payload_end > self.bytes.len() {
            return Err(self.corrupt("leaf payload out of range"));
        }
        let key = self.decode_item(payload, ksize, eflags & P_BIGKEY != 0)?;
        let data = self.decode_item(payload + ksize, dsize, eflags & P_BIGDATA != 0)?;
        Ok(LeafEntry { key, data })
    }

    /// Decode the internal entry at `idx`.
    ///
    /// # Errors
    ///
    /// Same conditions as [`PageView::leaf_entry`].
    pub(crate) fn internal_entry(&self, idx: usize) -> Result<InternalEntry> {
        let off = self.entry_offset(idx)?;
        let ksize = self.u32_at(off)? as usize;
        let child = self.u32_at(off + 4)?;
        let eflags = self.byte_at(off + 8)?;
        let payload = off + BINTERNAL_HEADER_SIZE;
        let payload_end = payload
            .checked_add(ksize)
            .ok_or_else(|| self.corrupt("internal payload length overflows"))?;
        if payload_end > self.bytes.len() {
            return Err(self.corrupt("internal payload out of range"));
        }
        let key = self.decode_item(payload, ksize, eflags & P_BIGKEY != 0)?;
        Ok(InternalEntry { key, child })
    }

    /// Byte offset of entry `idx` within the page, as recorded in
    /// `linp[idx]`.
    ///
    /// The offset is not validated for range or alignment here;
    /// downstream entry decoders bounds-check the payload they
    /// actually read.  This keeps page parsing constant-time
    /// regardless of `nentries`.
    fn entry_offset(&self, idx: usize) -> Result<usize> {
        if idx >= self.nentries {
            return Err(self.corrupt("entry index out of range"));
        }
        Ok(self.u16_at(PAGE_HEADER_SIZE + idx * 2)? as usize)
    }

    /// Decode an item slot at `[off, off + len)`, treating it as an
    /// overflow reference if `big` is set.
    ///
    /// `off` and `len` come from page-internal arithmetic bounded by
    /// `PageView::bytes.len() <= MAX_PSIZE = 32768`, so the casts to
    /// `u32` cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    fn decode_item(&self, off: usize, len: usize, big: bool) -> Result<ItemRef> {
        if !big {
            return Ok(ItemRef::Inline {
                off: off as u32,
                len: len as u32,
            });
        }
        if len < NOVFLSIZE {
            return Err(self.corrupt("overflow ref too short"));
        }
        let pgno = self.u32_at(off)?;
        let size = self.u32_at(off + 4)?;
        Ok(ItemRef::Overflow { pgno, size })
    }

    #[inline]
    fn byte_at(&self, off: usize) -> Result<u8> {
        self.bytes
            .get(off)
            .copied()
            .ok_or_else(|| self.corrupt("byte read out of range"))
    }

    #[inline]
    fn u16_at(&self, off: usize) -> Result<u16> {
        let end = off
            .checked_add(2)
            .ok_or_else(|| self.corrupt("u16 offset overflows"))?;
        let b = self
            .bytes
            .get(off..end)
            .ok_or_else(|| self.corrupt("u16 read out of range"))?;
        Ok(read_u16_bytes([b[0], b[1]], self.swap))
    }

    #[inline]
    fn u32_at(&self, off: usize) -> Result<u32> {
        let end = off
            .checked_add(4)
            .ok_or_else(|| self.corrupt("u32 offset overflows"))?;
        let b = self
            .bytes
            .get(off..end)
            .ok_or_else(|| self.corrupt("u32 read out of range"))?;
        Ok(read_u32_bytes([b[0], b[1], b[2], b[3]], self.swap))
    }

    #[inline]
    const fn corrupt(&self, reason: &'static str) -> Error {
        Error::CorruptPage {
            pgno: self.pgno,
            reason,
        }
    }
}

/// Read an unaligned `u16` from `bytes[off..]`, swapping bytes if the
/// file's byte order differs from the host.
#[inline]
fn read_u16(bytes: &[u8], off: usize, swap: bool) -> u16 {
    read_u16_bytes([bytes[off], bytes[off + 1]], swap)
}

/// Read an unaligned `u32` from `bytes[off..]`, swapping bytes if the
/// file's byte order differs from the host.
#[inline]
fn read_u32(bytes: &[u8], off: usize, swap: bool) -> u32 {
    read_u32_bytes(
        [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]],
        swap,
    )
}

#[inline]
const fn read_u16_bytes(b: [u8; 2], swap: bool) -> u16 {
    let v = u16::from_ne_bytes(b);
    if swap { v.swap_bytes() } else { v }
}

#[inline]
const fn read_u32_bytes(b: [u8; 4], swap: bool) -> u32 {
    let v = u32::from_ne_bytes(b);
    if swap { v.swap_bytes() } else { v }
}
