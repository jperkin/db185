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
 * On-disk constants and layout helpers.
 *
 * All field widths and offsets here are taken from `libnbcompat`'s
 * `nbcompat/db.h` and `db/btree/btree.h`.  See those headers for the
 * canonical definitions.
 *
 * The full set of constants is kept here even when the read-only path
 * does not yet reference all of them; the constants serve as the
 * format specification for the upcoming write side.
 */

#![allow(dead_code)]

/// Magic number at the start of `BTMETA` (page 0).  Stored in host byte
/// order; mismatch on read indicates the file was written with the
/// opposite endianness and requires byte-swapping.
pub(crate) const BTREE_MAGIC: u32 = 0x0005_3162;

/// Btree on-disk version.  Only this version is supported.
pub(crate) const BTREE_VERSION: u32 = 3;

/// Smallest legal page size.
pub(crate) const MIN_PSIZE: usize = 512;

/// Largest legal page size.  Page offsets are stored in `indx_t` which is
/// `u16`, so a page may be at most 65536 bytes.
pub(crate) const MAX_PSIZE: usize = 65536;

/// Reserved page number used for both the metadata page and for "no
/// page" sentinels in sibling / overflow links.
pub(crate) const P_INVALID: u32 = 0;

/// Page number of the `BTMETA` page.
pub(crate) const P_META: u32 = 0;

/// Page number of the root of the btree.
pub(crate) const P_ROOT: u32 = 1;

/// Page flag: internal page of a btree.
pub(crate) const P_BINTERNAL: u32 = 0x01;
/// Page flag: leaf page of a btree.
pub(crate) const P_BLEAF: u32 = 0x02;
/// Page flag: overflow page (a member of a key or value overflow chain).
pub(crate) const P_OVERFLOW: u32 = 0x04;
/// Page flag: internal page of a recno tree.  Unsupported.
pub(crate) const P_RINTERNAL: u32 = 0x08;
/// Page flag: leaf page of a recno tree.  Unsupported.
pub(crate) const P_RLEAF: u32 = 0x10;
/// Mask of all page type bits.
pub(crate) const P_TYPE: u32 = 0x1f;

/// Entry flag: the data lives on an overflow chain and the entry's
/// data slot is an 8-byte `{pgno, size}` reference.  Only meaningful
/// for [`P_BLEAF`] entries.
pub(crate) const P_BIGDATA: u8 = 0x01;
/// Entry flag: the key lives on an overflow chain and the entry's key
/// slot is an 8-byte `{pgno, size}` reference.  Applies to both
/// [`P_BLEAF`] and [`P_BINTERNAL`] entries.
pub(crate) const P_BIGKEY: u8 = 0x02;

/// `BTMETA` flag: duplicate keys are forbidden.
pub(crate) const B_NODUPS: u32 = 0x0020;
/// `BTMETA` flag: this is a recno tree.  Unsupported.
pub(crate) const R_RECNO: u32 = 0x0080;
/// All metadata flag bits we know how to interpret.
pub(crate) const SAVEMETA: u32 = B_NODUPS | R_RECNO;

/// Size of the `BTMETA` structure: six `u32` fields.
pub(crate) const META_SIZE: usize = 24;

/// Size of the `PAGE` header (the fixed part before `linp[]`).
/// `pgno`, `prevpg`, `nextpg`, `flags` (each `u32`), `lower`, `upper`
/// (each `u16`).
pub(crate) const PAGE_HEADER_SIZE: usize = 4 * 4 + 2 * 2;

/// Size of an overflow reference: `{pgno_t, u32 size}`.
pub(crate) const NOVFLSIZE: usize = 8;

/// Size of a `BLEAF` entry header before the `bytes[]` payload:
/// `ksize` (`u32`), `dsize` (`u32`), `flags` (`u8`).
pub(crate) const BLEAF_HEADER_SIZE: usize = 4 + 4 + 1;

/// Size of a `BINTERNAL` entry header before the `bytes[]` payload:
/// `ksize` (`u32`), `pgno` (`u32`), `flags` (`u8`).
pub(crate) const BINTERNAL_HEADER_SIZE: usize = 4 + 4 + 1;

/// Alignment of entries within a page.  Entries are aligned to
/// `sizeof(pgno_t)` (4 bytes) so the `pgno_t`-typed fields inside them
/// can be accessed without unaligned loads.
pub(crate) const ENTRY_ALIGN: usize = 4;

/// Round `n` up to the next multiple of [`ENTRY_ALIGN`].  Equivalent to
/// the `BTLALIGN` macro in `btree.h`.
#[inline]
pub(crate) const fn align_entry(n: usize) -> usize {
    (n + ENTRY_ALIGN - 1) & !(ENTRY_ALIGN - 1)
}

/// Byte layout of the `BTMETA` page (page 0).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Meta {
    /// Magic number.
    pub(crate) magic: u32,
    /// Format version.
    pub(crate) version: u32,
    /// Page size in bytes.
    pub(crate) psize: u32,
    /// Head of the free-page chain, or [`P_INVALID`].
    pub(crate) free: u32,
    /// Number of records (recno only; zero for btree).
    pub(crate) nrecs: u32,
    /// Metadata flag bits (a subset of [`SAVEMETA`]).
    pub(crate) flags: u32,
}

impl Meta {
    /// Decode a meta page from the first [`META_SIZE`] bytes of the
    /// file.  Returns the parsed meta and `true` if the file is in the
    /// opposite endianness to the host.
    pub(crate) fn parse(buf: &[u8; META_SIZE]) -> (Self, bool) {
        let native_magic = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let swap = native_magic != BTREE_MAGIC;
        let r = |off: usize| -> u32 {
            let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
            if swap {
                u32::from_ne_bytes(b).swap_bytes()
            } else {
                u32::from_ne_bytes(b)
            }
        };
        let meta = Self {
            magic: r(0),
            version: r(4),
            psize: r(8),
            free: r(12),
            nrecs: r(16),
            flags: r(20),
        };
        (meta, swap)
    }
}
