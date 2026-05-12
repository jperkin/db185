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
 * Database handle: mmap, meta-page validation, tree descent.
 *
 * The whole file is `mmap`ed on [`Db::open`].  Page access is then a
 * bounds-checked slice into the mapped region; there is no per-page
 * heap allocation and no cache to manage.  All read operations take
 * `&self`, so multiple iterators and lookups can run concurrently
 * against the same `Db`.
 */

use crate::Result;
use crate::error::Error;
use crate::format::{
    BTREE_MAGIC, BTREE_VERSION, MAX_PSIZE, META_SIZE, MIN_PSIZE, Meta, P_INVALID, P_ROOT,
    PAGE_HEADER_SIZE, R_RECNO, SAVEMETA,
};
use crate::iter::{Entry, Iter};
use crate::page::{ItemRef, PageView};
use memmap2::Mmap;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fs::File;
use std::path::Path;

/// Open handle on a btree database file.
///
/// Wraps a read-only `mmap` of the file plus the parameters decoded
/// from its meta page (page size, host/foreign byte order).
///
/// Because the file is held via `mmap`, the caller is responsible for
/// ensuring it is not mutated by another process for the lifetime of
/// the `Db`.  External writes can observably change bytes the API
/// hands out and would constitute undefined behaviour at the `mmap`
/// layer.  For the typical pkgsrc use case - a single short-lived
/// reader over `pkgdb.byfile.db` - this is trivially satisfied.
#[derive(Debug)]
pub struct Db {
    mmap: Mmap,
    psize: usize,
    swap: bool,
}

impl Db {
    /// Open `path` for reading.
    ///
    /// Maps the file with `mmap(2)` and validates the meta page.
    /// Recno trees are rejected; btree pages themselves are
    /// validated lazily on first access.
    ///
    /// See the [`Db`] type-level documentation for the `mmap`
    /// concurrent-writer invariant the caller must uphold.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be opened or mapped,
    /// or [`Error::ShortFile`] if it is too small to contain a meta
    /// page.  Returns [`Error::BadMagic`], [`Error::BadVersion`],
    /// [`Error::BadPageSize`], [`Error::UnsupportedFlags`] or
    /// [`Error::UnalignedFileLength`] when the meta page is
    /// recognisable but its contents fall outside the supported
    /// subset.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        // SAFETY: see the `Db` type-level docs - the caller is
        // responsible for ensuring no concurrent writers to the file.
        let mmap = unsafe { Mmap::map(&file)? };

        let hdr: &[u8; META_SIZE] = mmap.first_chunk().ok_or(Error::ShortFile)?;
        let (meta, swap) = Meta::parse(hdr);

        if meta.magic != BTREE_MAGIC {
            return Err(Error::BadMagic { magic: meta.magic });
        }
        if meta.version != BTREE_VERSION {
            return Err(Error::BadVersion {
                version: meta.version,
            });
        }
        let psize = meta.psize as usize;
        if !(MIN_PSIZE..=MAX_PSIZE).contains(&psize) || (psize & 1) != 0 {
            return Err(Error::BadPageSize { psize: meta.psize });
        }
        if meta.flags & !SAVEMETA != 0 || meta.flags & R_RECNO != 0 {
            return Err(Error::UnsupportedFlags { flags: meta.flags });
        }
        if mmap.len() % psize != 0 {
            return Err(Error::UnalignedFileLength {
                len: mmap.len() as u64,
                psize: meta.psize,
            });
        }
        Ok(Self { mmap, psize, swap })
    }

    /// Look up a key.
    ///
    /// Returns `Ok(Some(value))` on hit and `Ok(None)` if the key is
    /// absent.  Inline values are returned as `Cow::Borrowed` slices
    /// into the mmap; overflow values are materialised into
    /// `Cow::Owned`.  Keys are compared as raw byte strings with
    /// unsigned byte-by-byte ordering and shorter-wins (matching
    /// `__bt_defcmp` in `bt_utils.c`, which is equivalent to
    /// `<[u8] as Ord>::cmp`).
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] on I/O failure or on-disk corruption
    /// encountered while descending the tree.
    pub fn get(&self, key: &[u8]) -> Result<Option<Cow<'_, [u8]>>> {
        let Some(pos) = self.search(key)? else {
            return Ok(None);
        };
        let view = PageView::parse(pos.pgno, self.page(pos.pgno)?, self.swap)?;
        let entry = view.leaf_entry(pos.index)?;
        Ok(Some(self.materialise(&view, entry.data)?))
    }

    /// Begin a forward iteration over all key/value pairs in sorted
    /// order.  Equivalent to `seq(R_FIRST)` followed by repeated
    /// `seq(R_NEXT)` in the historical `dbopen(3)` API.
    #[must_use]
    pub const fn iter(&self) -> Iter<'_> {
        Iter::new(self)
    }

    /// Borrow the bytes of page `pgno`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PageOutOfBounds`] if `pgno` lies past the end
    /// of the mapped file.
    #[inline]
    pub(crate) fn page(&self, pgno: u32) -> Result<&[u8]> {
        let start = pgno as usize * self.psize;
        let end = start + self.psize;
        self.mmap
            .get(start..end)
            .ok_or(Error::PageOutOfBounds { pgno })
    }

    /// `true` if the on-disk file is in the opposite endianness to
    /// the host CPU.
    #[inline]
    pub(crate) const fn swap(&self) -> bool {
        self.swap
    }

    /// Descend the tree from the root to the leaf that would contain
    /// `key`, returning an exact-match position or `None`.
    fn search(&self, key: &[u8]) -> Result<Option<Position>> {
        let mut pgno = P_ROOT;
        loop {
            let view = PageView::parse(pgno, self.page(pgno)?, self.swap)?;
            if view.is_leaf() {
                return self.search_leaf(&view, key);
            }
            if !view.is_internal() {
                return Err(Error::CorruptPage {
                    pgno,
                    reason: "non-internal page during descent",
                });
            }
            pgno = self.descend_internal(&view, key)?;
        }
    }

    /// Binary search a leaf page for an exact match.  Mirrors the
    /// leaf case of `__bt_search` in `bt_search.c`.
    fn search_leaf(&self, view: &PageView<'_>, key: &[u8]) -> Result<Option<Position>> {
        let mut base = 0usize;
        let mut lim = view.nentries();
        while lim > 0 {
            let idx = base + (lim >> 1);
            let entry = view.leaf_entry(idx)?;
            match self.compare_item(key, view, entry.key)? {
                Ordering::Equal => {
                    return Ok(Some(Position {
                        pgno: view.pgno,
                        index: idx,
                    }));
                }
                Ordering::Greater => {
                    base = idx + 1;
                    lim -= 1;
                }
                Ordering::Less => {}
            }
            lim >>= 1;
        }
        Ok(None)
    }

    /// Decide which child of an internal page to descend into for
    /// `key`.  On the leftmost internal page of any level (the one
    /// with `prevpg == P_INVALID`), the separator at index 0 compares
    /// as "less than any key" - matching the special case in
    /// `__bt_cmp` that keeps the leftmost path always descendable.
    fn descend_internal(&self, view: &PageView<'_>, key: &[u8]) -> Result<u32> {
        let leftmost_spine = view.prev_pg == P_INVALID;
        let mut base = 0usize;
        let mut lim = view.nentries();
        let mut matched: Option<usize> = None;
        while lim > 0 {
            let idx = base + (lim >> 1);
            let entry = view.internal_entry(idx)?;
            let cmp = if idx == 0 && leftmost_spine {
                Ordering::Greater
            } else {
                self.compare_item(key, view, entry.key)?
            };
            match cmp {
                Ordering::Equal => {
                    matched = Some(idx);
                    break;
                }
                Ordering::Greater => {
                    base = idx + 1;
                    lim -= 1;
                }
                Ordering::Less => {}
            }
            lim >>= 1;
        }
        let chosen = match matched {
            Some(idx) => idx,
            None if base == 0 => 0,
            None => base - 1,
        };
        Ok(view.internal_entry(chosen)?.child)
    }

    /// Compare a user key against an [`ItemRef`] decoded from `view`.
    ///
    /// Inline items compare directly against the borrowed slice.
    /// Overflow items stream through the page chain a page at a
    /// time, comparing each segment without allocating the full
    /// key into a temporary `Vec`.  That matters for hot lookup
    /// paths: a binary search at a non-trivial level would
    /// otherwise allocate (and re-fetch) the whole key on every
    /// comparison.
    fn compare_item(&self, key: &[u8], view: &PageView<'_>, item: ItemRef) -> Result<Ordering> {
        match item {
            ItemRef::Inline { .. } => Ok(key.cmp(view.inline(item)?)),
            ItemRef::Overflow { pgno, size } => self.compare_overflow(key, pgno, size),
        }
    }

    /// Streaming comparison of `key` against the overflow chain
    /// starting at `start_pgno` whose total declared payload
    /// length is `size`.  Walks at most as many pages as needed
    /// to find the first differing byte (or to exhaust the
    /// shorter side), without ever materialising the full
    /// overflow key into a `Vec`.  Matches `<[u8] as Ord>::cmp`
    /// semantics: byte-by-byte, then shorter-wins.
    fn compare_overflow(&self, key: &[u8], start_pgno: u32, size: u32) -> Result<Ordering> {
        let payload_per_page = self.psize - PAGE_HEADER_SIZE;
        let total = size as usize;
        let mut pgno = start_pgno;
        let mut consumed = 0usize;
        while consumed < total {
            if pgno == P_INVALID {
                return Err(Error::CorruptOverflow { pgno: start_pgno });
            }
            let view = PageView::parse(pgno, self.page(pgno)?, self.swap)?;
            if !view.is_overflow() {
                return Err(Error::CorruptOverflow { pgno: start_pgno });
            }
            let take = (total - consumed).min(payload_per_page);
            let payload = view.overflow_payload();
            if take > payload.len() {
                return Err(Error::CorruptOverflow { pgno: start_pgno });
            }
            let other = &payload[..take];

            if consumed >= key.len() {
                // `key` ran out earlier; the overflow side has
                // more bytes to come -> `key` is shorter.
                return Ok(Ordering::Less);
            }
            let key_rest = &key[consumed..];
            let cmp_len = key_rest.len().min(other.len());
            match key_rest[..cmp_len].cmp(&other[..cmp_len]) {
                Ordering::Equal => {}
                ord => return Ok(ord),
            }
            if key_rest.len() < other.len() {
                // `key` exhausted partway through this page.
                return Ok(Ordering::Less);
            }
            consumed += take;
            pgno = view.next_pg;
        }
        // Whole overflow consumed without a mismatch; final
        // ordering is determined by whether `key` had extra
        // trailing bytes.
        Ok(key.len().cmp(&total))
    }

    /// Resolve an [`ItemRef`] decoded from `view` into a borrowed or
    /// owned byte slice, fetching overflow pages when necessary.
    pub(crate) fn materialise<'a>(
        &'a self,
        view: &PageView<'a>,
        item: ItemRef,
    ) -> Result<Cow<'a, [u8]>> {
        match item {
            ItemRef::Inline { .. } => Ok(Cow::Borrowed(view.inline(item)?)),
            ItemRef::Overflow { pgno, size } => Ok(Cow::Owned(self.fetch_overflow(pgno, size)?)),
        }
    }

    /// Read a key or data item out of an overflow chain.
    ///
    /// The chain starts at page `start_pgno` and is linked by each
    /// page's `nextpg` field.  Each page contributes up to
    /// `psize - PAGE_HEADER_SIZE` bytes of payload (the C macro
    /// `BTDATAOFF`).  Mirrors `__ovfl_get` in `bt_overflow.c`.
    fn fetch_overflow(&self, start_pgno: u32, size: u32) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(size as usize);
        let payload_per_page = self.psize - PAGE_HEADER_SIZE;
        let mut pgno = start_pgno;
        let mut remaining = size as usize;
        while remaining > 0 {
            if pgno == P_INVALID {
                return Err(Error::CorruptOverflow { pgno: start_pgno });
            }
            let view = PageView::parse(pgno, self.page(pgno)?, self.swap)?;
            if !view.is_overflow() {
                return Err(Error::CorruptOverflow { pgno: start_pgno });
            }
            let take = remaining.min(payload_per_page);
            let payload = view.overflow_payload();
            if take > payload.len() {
                return Err(Error::CorruptOverflow { pgno: start_pgno });
            }
            out.extend_from_slice(&payload[..take]);
            remaining -= take;
            pgno = view.next_pg;
        }
        Ok(out)
    }
}

/// Location of a key/value pair within the tree.
#[derive(Clone, Copy)]
struct Position {
    pgno: u32,
    index: usize,
}

impl<'a> IntoIterator for &'a Db {
    type Item = Result<Entry<'a>>;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
