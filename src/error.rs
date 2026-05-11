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

use std::io;
use thiserror::Error;

/// Errors returned by this crate.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Underlying I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// File is shorter than the minimum required to hold a `BTMETA`
    /// page.  Distinct from [`Error::Io`] so callers can distinguish
    /// "wrong file type" from "transient I/O failure".
    #[error("file shorter than btree meta page")]
    ShortFile,

    /// File does not begin with a recognised `BTMETA` header.
    #[error("not a btree 1.85 file: bad magic {magic:#010x}")]
    BadMagic {
        /// The 32-bit value found where the magic was expected.
        magic: u32,
    },

    /// Header magic matches but version is unsupported.  Only version 3
    /// is supported, matching `BTREEVERSION` from `nbcompat/db.h`.
    #[error("unsupported btree version {version} (expected 3)")]
    BadVersion {
        /// Version read from the meta page.
        version: u32,
    },

    /// Page size in the metadata is outside the supported range or is not
    /// a multiple of `sizeof(indx_t)`.
    #[error("invalid page size {psize}")]
    BadPageSize {
        /// Page size read from the meta page, in bytes.
        psize: u32,
    },

    /// Metadata flags include bits this implementation cannot interpret.
    /// Currently only `B_NODUPS` and `R_RECNO` are recognised, and
    /// `R_RECNO` indicates a recno tree which we do not support.
    #[error("unsupported metadata flags {flags:#x}")]
    UnsupportedFlags {
        /// Raw flag word read from the meta page.
        flags: u32,
    },

    /// File is shorter than the page being requested.
    #[error("page {pgno} is past end of file")]
    PageOutOfBounds {
        /// Page number that was requested.
        pgno: u32,
    },

    /// Page header or entry layout is internally inconsistent.
    #[error("corrupt page {pgno}: {reason}")]
    CorruptPage {
        /// Page number whose contents failed validation.
        pgno: u32,
        /// Short description of the structural problem.
        reason: &'static str,
    },

    /// Overflow chain is malformed: it ends before all bytes were
    /// read, or includes a page whose flags are not `P_OVERFLOW`.
    #[error("corrupt overflow chain starting at page {pgno}")]
    CorruptOverflow {
        /// First page number of the malformed chain.
        pgno: u32,
    },
}
