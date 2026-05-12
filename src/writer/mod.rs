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
 * Build a fresh btree file from a sequence of [`Writer::put`] /
 * [`Writer::del`] operations.
 *
 * Pages are staged in an in-memory arena indexed by page number and
 * written on finish, meta page first.  Split decisions and prefix
 * compression follow `libnbcompat`'s `bt_split.c`, so output matches
 * a fresh `dbopen(3)` rebuild byte-for-byte in active bytes (page
 * headers, `linp[]`, entry headers + keys + values); entry-alignment
 * padding and free-space slack are unspecified.
 *
 * Overflow keys/values are not supported; oversize entries return
 * [`Error::EntryTooLarge`].
 *
 * [`Error::EntryTooLarge`]: crate::Error::EntryTooLarge
 */

mod page_buf;

use crate::Result;
use crate::format::{
    B_NODUPS, BINTERNAL_HEADER_SIZE, BLEAF_HEADER_SIZE, BTREE_MAGIC, BTREE_VERSION, MAX_PSIZE,
    P_BINTERNAL, P_BLEAF, P_INVALID, P_ROOT, P_TYPE, PAGE_HEADER_SIZE, align_entry,
};
use crate::page::PageView;
use page_buf::{PageBufMut, PageBufRef};
use std::cmp::Ordering;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

/// Default page size used for new files.  Matches `pkg_install`,
/// which always passes `psize = 4096` to `dbopen`.
const DEFAULT_PSIZE: usize = 4096;

/// Builder for a new btree file.
///
/// `Writer::create_new(path)` initialises the in-memory state with
/// an empty root leaf; `finish()` flushes that plus the meta page
/// to disk.  Dropping a `Writer` without calling `finish()` leaves
/// the newly-created file empty on disk - nothing is written until
/// finish.
#[must_use = "Writer is staged in memory only; call finish() to write the database"]
pub struct Writer {
    file: File,
    psize: usize,
    /// Flat in-memory page arena: a single `Vec<u8>` holding every
    /// page back-to-back, indexed by `pgno * psize`.  Slot 0 (the
    /// meta page) is built fresh into the arena head at finish;
    /// slot 1 is the root.  Pages are always allocated in
    /// increasing pgno order (each new page is appended as
    /// `psize` more bytes onto the end), so a flat buffer is both
    /// the natural shape for `O(1)` random access during descent
    /// and the cheapest possible flush in [`Writer::finish`]: a
    /// single buffered write of the whole arena.
    pages: Vec<u8>,
    /// Head of the free-page chain, recorded into the meta page.
    free: u32,
    /// Record count, recorded into the meta page.  Always zero for
    /// btree trees; only recno uses this field.
    nrecs: u32,
    /// Persistent flag bits (`B_NODUPS`, `R_RECNO`).  We only set
    /// `B_NODUPS` to match `pkg_install`.
    flags: u32,
}

impl Writer {
    /// Slice of the flat arena holding page `pgno`.
    fn page_bytes(&self, pgno: u32) -> &[u8] {
        let start = pgno as usize * self.psize;
        &self.pages[start..start + self.psize]
    }

    /// Mutable slice of the flat arena holding page `pgno`.
    fn page_bytes_mut(&mut self, pgno: u32) -> &mut [u8] {
        let start = pgno as usize * self.psize;
        let psize = self.psize;
        &mut self.pages[start..start + psize]
    }

    /// Borrow page `pgno` as a [`PageBufRef`].
    ///
    /// Panics if `pgno` is out of range, which would indicate
    /// writer-internal inconsistency.
    fn page(&self, pgno: u32) -> PageBufRef<'_> {
        PageBufRef::new(self.page_bytes(pgno))
    }

    /// Borrow page `pgno` mutably as a [`PageBufMut`].
    ///
    /// Panics if `pgno` is out of range, which would indicate
    /// writer-internal inconsistency.
    fn page_mut(&mut self, pgno: u32) -> PageBufMut<'_> {
        PageBufMut::new(self.page_bytes_mut(pgno))
    }

    /// The pgno that the next [`Writer::push_page`] would land at.
    /// Split paths predict `r_pgno` (and sometimes `l_pgno`) up
    /// front so the in-flight build references the correct values
    /// before the arena is actually grown.
    fn next_free_pgno(&self) -> u32 {
        let count = self.pages.len() / self.psize;
        u32::try_from(count).expect("page count fits in u32")
    }

    /// Append `buf` (which must be exactly `psize` bytes) as the
    /// next page in the arena, returning its pgno.  Phase-2 commit
    /// helper for splits.
    fn push_page(&mut self, buf: &[u8]) -> u32 {
        debug_assert_eq!(buf.len(), self.psize);
        let pgno = self.next_free_pgno();
        self.pages.extend_from_slice(buf);
        pgno
    }

    /// Overwrite page `pgno` in the arena with `buf` (which must
    /// be exactly `psize` bytes).  Phase-2 commit helper for the
    /// "left half stays at the original pgno" half of a split.
    fn replace_page(&mut self, pgno: u32, buf: &[u8]) {
        debug_assert_eq!(buf.len(), self.psize);
        self.page_bytes_mut(pgno).copy_from_slice(buf);
    }

    /// Create a new file at `path` and stage an empty btree.
    ///
    /// Mirrors [`std::fs::File::create_new`]: the file is opened
    /// with `create_new`, so an existing file at `path` causes an
    /// I/O error.  Nothing is written to disk until
    /// [`Writer::finish`] is called.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be created.
    ///
    /// [`Error::Io`]: crate::Error::Io
    pub fn create_new(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let psize = DEFAULT_PSIZE;
        // Slot 0 is reserved for the meta page (built fresh at
        // close); slot 1 (`P_ROOT`) is the initial empty leaf root.
        // Reserve room for ~64 pages so small databases don't
        // re-allocate during construction.
        let mut pages: Vec<u8> = Vec::with_capacity(64 * psize);
        pages.resize(2 * psize, 0);
        pages[psize..2 * psize].copy_from_slice(&empty_leaf(P_ROOT, psize));
        Ok(Self {
            file,
            psize,
            pages,
            free: P_INVALID,
            nrecs: 0,
            flags: B_NODUPS,
        })
    }

    /// Delete the entry for `key` from the tree.
    ///
    /// Returns `true` if an entry was removed, `false` if `key`
    /// wasn't present.  Mirrors `__bt_delete` / `__bt_dleaf` from
    /// libnbcompat: the leaf is compacted in place and the tree's
    /// `nrecs` field is left alone (the btree access method ignores
    /// it).  Page underflow doesn't trigger merges - the BSD btree
    /// just leaves underfull pages alone.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if any in-memory page fails its own
    /// decoder invariants while descending to find `key`.  That
    /// would only happen if the writer's own state were corrupted,
    /// which indicates a bug in this crate, not bad input.
    ///
    /// # Panics
    ///
    /// Panics if the writer's internal page map is inconsistent
    /// (i.e. a page reached via descent isn't present in the map).
    /// Reaching this branch indicates a bug in this crate.
    ///
    /// [`Error`]: crate::Error
    pub fn del(&mut self, key: &[u8]) -> Result<bool> {
        let spine = self.descend(key)?;
        let leaf_pgno = spine.leaf_pgno;
        let (exact, idx) = leaf_search(leaf_pgno, self.page(leaf_pgno), key)?;
        if !exact {
            return Ok(false);
        }
        delete_leaf_entry(&mut self.page_mut(leaf_pgno), idx);
        Ok(true)
    }

    /// Insert `key` / `val` into the tree.
    ///
    /// Mirrors `dbopen(3)`'s `R_NOOVERWRITE` semantics: returns
    /// `Ok(true)` on a successful insert, or `Ok(false)` if `key`
    /// is already present (in which case the existing value is
    /// left untouched).  Matches [`std::collections::HashSet::insert`].
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if any in-memory page fails its own
    /// decoder invariants while descending to find the insertion
    /// point.  That would only happen if the writer's own state
    /// were corrupted, which indicates a bug in this crate, not bad
    /// input.
    ///
    /// # Panics
    ///
    /// Panics if the writer's internal page map is inconsistent
    /// (i.e. a page reached via descent isn't present in the map).
    /// Reaching this branch indicates a bug in this crate.
    ///
    /// [`Error`]: crate::Error
    pub fn put(&mut self, key: &[u8], val: &[u8]) -> Result<bool> {
        let psize = self.psize;
        // Reject entries that we couldn't represent inline anywhere
        // in the tree without `P_BIGKEY` / `P_BIGDATA` overflow
        // chain support.  Two bounds matter (both detailed in the
        // [`Error::EntryTooLarge`] doc):
        //
        //   1. The leaf bound: the `BLEAF` entry plus its `linp[]`
        //      slot must fit a fresh empty leaf.
        //   2. The separator bound: the key may become a
        //      `BINTERNAL` separator at every level above the
        //      leaf.  The most constrained context is the root
        //      conversion, which packs the new separator alongside
        //      a zero-key entry and two `linp` slots on a fresh
        //      page.
        //
        // Checking both up front means in-tree split paths can
        // assume every inline entry survives every operation and
        // can never produce a mid-split unfittable layout.  In
        // principle a key that fits a leaf but not the separator
        // bound is C-valid storage; in practice the C side has
        // a similar pathological corner (a single near-maximal
        // leaf entry leaves no room for any sibling, so adding
        // one drives `bt_psplit` into an unfittable plan), so
        // the strictness here costs no real-world compatibility.
        //
        // [`Error::EntryTooLarge`]: crate::Error::EntryTooLarge
        let leaf_bytes = align_entry(BLEAF_HEADER_SIZE + key.len() + val.len());
        let sep_bytes = align_entry(BINTERNAL_HEADER_SIZE + key.len());
        let zero_key_bytes = align_entry(BINTERNAL_HEADER_SIZE);
        let usable = psize - PAGE_HEADER_SIZE;
        if leaf_bytes + 2 > usable || sep_bytes + zero_key_bytes + 4 > usable {
            return Err(crate::Error::EntryTooLarge {
                key_len: key.len(),
                val_len: val.len(),
            });
        }
        let spine = self.descend(key)?;
        let leaf_pgno = spine.leaf_pgno;

        let leaf = self.page(leaf_pgno);
        let (exact, idx) = leaf_search(leaf_pgno, leaf, key)?;
        if exact {
            return Ok(false);
        }

        if leaf.free_space() >= leaf_bytes + 2 {
            insert_leaf_entry(&mut self.page_mut(leaf_pgno), idx, key, val);
            return Ok(true);
        }

        if leaf_pgno == P_ROOT {
            self.split_root_leaf(idx, key, val)?;
            return Ok(true);
        }

        // Non-root leaf split.  Produces a separator that must be
        // inserted into the parent (and possibly propagated further
        // up the tree).
        let sep = self.split_non_root_leaf(leaf_pgno, idx, key, val)?;
        self.propagate_separator(spine.path, sep)?;
        Ok(true)
    }

    /// Descend the tree from the root, returning a [`Spine`] that
    /// records every (internal-page, slot-index) hop and the leaf at
    /// the bottom.
    fn descend(&self, key: &[u8]) -> Result<Spine> {
        let mut path = Vec::new();
        let mut current = P_ROOT;
        loop {
            let page = self.page(current);
            if page.is_leaf() {
                return Ok(Spine {
                    path,
                    leaf_pgno: current,
                });
            }
            let view = PageView::parse(current, page.bytes(), false)?;
            let leftmost = view.prev_pg == P_INVALID;
            let chosen = descend_internal_pick(&view, key, leftmost)?;
            let child = view.internal_entry(chosen)?.child;
            path.push((current, chosen));
            current = child;
        }
    }

    /// Split a non-root leaf and return the separator that the
    /// parent-insert path needs to add.
    fn split_non_root_leaf(
        &mut self,
        leaf_pgno: u32,
        skip: usize,
        key: &[u8],
        val: &[u8],
    ) -> Result<Separator> {
        let leaf = self.page(leaf_pgno);
        if leaf.is_rightmost() && skip == leaf.nentries() {
            self.split_leaf_sorted_opt(leaf_pgno, key, val)
        } else {
            self.split_leaf_full(leaf_pgno, skip, key, val)
        }
    }

    /// `bt_page`'s sorted-insert optimisation: rightmost leaf,
    /// appending new key.  Returns the separator info for the parent
    /// (the new key itself, with the existing leaf's last key
    /// supplied for prefix compression).
    fn split_leaf_sorted_opt(
        &mut self,
        leaf_pgno: u32,
        key: &[u8],
        val: &[u8],
    ) -> Result<Separator> {
        let psize = self.psize;

        let l_last_key = {
            let leaf = self.page(leaf_pgno);
            let view = PageView::parse(leaf_pgno, leaf.bytes(), false)?;
            let n = view.nentries();
            view.inline(view.leaf_entry(n - 1)?.key)?.to_vec()
        };

        let r_pgno = self.next_free_pgno();
        let mut r_buf = empty_leaf(r_pgno, psize);
        {
            let mut r = PageBufMut::new(&mut r_buf);
            r.set_prev_pg(leaf_pgno);
            r.set_lower(PAGE_HEADER_SIZE + 2);
            write_entry_at_slot(&mut r, 0, key, val);
        }

        self.page_mut(leaf_pgno).set_next_pg(r_pgno);
        self.push_page(&r_buf);
        Ok(Separator::Leaf {
            sep_key: key.to_vec(),
            child: r_pgno,
            l_last_key,
        })
    }

    /// `bt_page`'s full split: distribute the leaf's entries between
    /// a fresh `l` (kept at the original `leaf_pgno`) and a fresh `r`
    /// at a new page number.
    fn split_leaf_full(
        &mut self,
        leaf_pgno: u32,
        skip: usize,
        key: &[u8],
        val: &[u8],
    ) -> Result<Separator> {
        let psize = self.psize;
        let ilen = align_entry(BLEAF_HEADER_SIZE + key.len() + val.len());

        // `r` will live at the next free slot.  We don't push it
        // onto the arena until after every fallible step succeeds,
        // so an early `?` can't leave a phantom page behind.
        let r_pgno = self.next_free_pgno();

        let mut l_buf = empty_leaf(leaf_pgno, psize);
        let mut r_buf = empty_leaf(r_pgno, psize);
        let leaf_next_pg;
        let (l_last_key, r_first_key) = {
            let old = self.page(leaf_pgno);
            let mut l = PageBufMut::new(&mut l_buf);
            let mut r = PageBufMut::new(&mut r_buf);
            l.set_prev_pg(old.prev_pg());
            l.set_next_pg(r_pgno);
            r.set_prev_pg(leaf_pgno);
            leaf_next_pg = old.next_pg();
            r.set_next_pg(leaf_next_pg);

            let view = PageView::parse(leaf_pgno, old.bytes(), false)?;
            let n = view.nentries();
            let plan = compute_psplit_split_leaf(&view, skip, ilen, psize)?;
            copy_entries(&view, 0..plan.h_in_l(), plan.l_skip(skip), &mut l)?;
            copy_entries(&view, plan.h_in_l()..n, plan.r_skip(skip), &mut r)?;

            let target = if plan.right_target { &mut r } else { &mut l };
            write_entry_at_slot(target, plan.target_slot(skip), key, val);

            (leaf_last_key(l.bytes())?, leaf_first_key(r.bytes())?)
        };

        // Phase 2: infallible commit.
        self.replace_page(leaf_pgno, &l_buf);
        let pushed = self.push_page(&r_buf);
        debug_assert_eq!(pushed, r_pgno);

        if leaf_next_pg != P_INVALID {
            self.page_mut(leaf_next_pg).set_prev_pg(r_pgno);
        }

        Ok(Separator::Leaf {
            sep_key: r_first_key,
            child: r_pgno,
            l_last_key,
        })
    }

    /// Walk the spine inserting `sep` at each level, splitting parent
    /// pages as needed.  Mirrors the parent-insert loop of
    /// `__bt_split`.
    fn propagate_separator(
        &mut self,
        mut spine: Vec<(u32, usize)>,
        mut sep: Separator,
    ) -> Result<()> {
        while let Some((parent_pgno, parent_idx)) = spine.pop() {
            let skip = parent_idx + 1;
            match self.try_insert_internal(parent_pgno, skip, &sep)? {
                None => return Ok(()),
                Some(new_sep) => sep = new_sep,
            }
        }
        // Reached above the root.  Shouldn't happen: the root case
        // is handled in `try_insert_internal` by promoting to a
        // brand-new internal root, which returns `None` immediately.
        unreachable!("propagate_separator ran past the root");
    }

    /// Insert `sep` at slot `skip` of `parent_pgno`.  Returns
    /// `None` if the insert fits in place or if a root split
    /// promoted a new internal root (no further propagation
    /// needed).  Returns `Some(new_sep)` when a non-root split
    /// produced a separator the caller must propagate to the
    /// grandparent.
    fn try_insert_internal(
        &mut self,
        parent_pgno: u32,
        skip: usize,
        sep: &Separator,
    ) -> Result<Option<Separator>> {
        let parent = self.page(parent_pgno);
        let (nksize, nbytes) = sep.encoded_size(parent.prev_pg(), skip);

        if parent.free_space() >= nbytes + 2 {
            insert_internal_in_place(
                &mut self.page_mut(parent_pgno),
                skip,
                nksize,
                sep.sep_key(),
                sep.child(),
            );
            return Ok(None);
        }

        if parent_pgno == P_ROOT {
            self.split_root_internal(skip, sep, nksize)?;
            return Ok(None);
        }

        // `bt_page`'s sorted-append shortcut applies to non-root
        // internal pages too: when the parent is the rightmost
        // page on its level (`nextpg == P_INVALID`) and the new
        // separator's slot is at the very end (`skip == NEXTINDEX`),
        // keep `parent` unchanged and allocate `r` holding only the
        // new separator.  Skipping this shortcut produces a balanced
        // bt_psplit instead, which packs entries very differently
        // from C and cascades into a deeper / wider upper tree.
        if parent.is_rightmost() && skip == parent.nentries() {
            return Ok(Some(self.split_internal_sorted_opt(
                parent_pgno,
                sep,
                nksize,
            )));
        }

        Ok(Some(self.split_internal_page(
            parent_pgno,
            skip,
            sep,
            nksize,
        )?))
    }

    /// `bt_page`'s sorted-append shortcut for internal pages.  The
    /// parent (the rightmost page on its level) keeps all existing
    /// entries; a fresh sibling `r` is allocated holding just the
    /// new separator at `linp[0]`.  Returns the separator that the
    /// grandparent insert path will use - the same separator, now
    /// pointing at `r` and carrying the ksize we wrote.
    fn split_internal_sorted_opt(
        &mut self,
        parent_pgno: u32,
        sep: &Separator,
        nksize: usize,
    ) -> Separator {
        let psize = self.psize;

        let r_pgno = self.next_free_pgno();
        let mut r_buf = empty_internal(r_pgno, psize);
        {
            let mut r = PageBufMut::new(&mut r_buf);
            r.set_prev_pg(parent_pgno);
            r.set_lower(PAGE_HEADER_SIZE + 2);
            write_internal_entry_at_slot(&mut r, 0, nksize, sep.sep_key(), sep.child());
        }

        self.page_mut(parent_pgno).set_next_pg(r_pgno);
        self.push_page(&r_buf);

        Separator::Internal {
            sep_key: sep.sep_key().to_vec(),
            child: r_pgno,
            ksize: nksize,
        }
    }

    /// Split the (internal) root.  Allocates two fresh internal
    /// pages, distributes the root's entries between them, places
    /// the new separator into whichever side owns its reserved slot,
    /// and converts the original root in place into a 2-entry
    /// internal pointing at the new pages.  Mirrors the root case of
    /// the parent-split path in `__bt_split` + `bt_root` + `bt_broot`.
    fn split_root_internal(&mut self, skip: usize, sep: &Separator, nksize: usize) -> Result<()> {
        let psize = self.psize;
        let ilen = align_entry(BINTERNAL_HEADER_SIZE + nksize);

        // l_pgno, r_pgno are the next two free slots.  Predicted
        // here so the in-place root conversion can reference them;
        // pushed onto the arena in phase 2 once every fallible step
        // succeeds.
        let l_pgno = self.next_free_pgno();
        let r_pgno = l_pgno + 1;

        let mut l_buf = empty_internal(l_pgno, psize);
        let mut r_buf = empty_internal(r_pgno, psize);
        {
            let mut l = PageBufMut::new(&mut l_buf);
            let mut r = PageBufMut::new(&mut r_buf);
            l.set_next_pg(r_pgno);
            r.set_prev_pg(l_pgno);

            let old_root = self.page(P_ROOT);
            let view = PageView::parse(P_ROOT, old_root.bytes(), false)?;
            let n = view.nentries();
            let plan = compute_psplit_split_internal(&view, skip, ilen, psize)?;

            copy_internal_entries(&view, 0..plan.h_in_l(), plan.l_skip(skip), &mut l)?;
            copy_internal_entries(&view, plan.h_in_l()..n, plan.r_skip(skip), &mut r)?;

            let target = if plan.right_target { &mut r } else { &mut l };
            write_internal_entry_at_slot(
                target,
                plan.target_slot(skip),
                nksize,
                sep.sep_key(),
                sep.child(),
            );
        }

        let r_first_key = first_internal_key(&r_buf)?;

        // Phase 2: build the in-place root mutation in a scratch
        // buffer first.  Cloning the root once is cheap (root
        // splits are rare and only happen at tree-grow events) and
        // lets the only remaining fallible step (the separator-fits
        // check inside `convert_root_to_internal`) happen against
        // a fresh buffer.  On Err the arena is still untouched.
        let mut new_root_buf = self.page_bytes(P_ROOT).to_vec();
        convert_root_to_internal(
            &mut PageBufMut::new(&mut new_root_buf),
            psize,
            l_pgno,
            r_pgno,
            &r_first_key,
        );

        let l_pushed = self.push_page(&l_buf);
        debug_assert_eq!(l_pushed, l_pgno);
        let r_pushed = self.push_page(&r_buf);
        debug_assert_eq!(r_pushed, r_pgno);
        self.replace_page(P_ROOT, &new_root_buf);
        Ok(())
    }

    /// Split a non-root internal page.  Distributes its entries
    /// between a fresh `l` (kept at the original `parent_pgno`) and
    /// a fresh `r` at a new page number, places the new separator,
    /// and returns a fresh [`Separator`] (from r's first internal
    /// entry, verbatim) so the caller can keep walking up the spine.
    fn split_internal_page(
        &mut self,
        parent_pgno: u32,
        skip: usize,
        sep: &Separator,
        nksize: usize,
    ) -> Result<Separator> {
        let psize = self.psize;
        let ilen = align_entry(BINTERNAL_HEADER_SIZE + nksize);

        // Predict r's pgno; only push after every fallible step
        // succeeds (see `split_leaf_full` for the rationale).
        let r_pgno = self.next_free_pgno();

        let mut l_buf = empty_internal(parent_pgno, psize);
        let mut r_buf = empty_internal(r_pgno, psize);
        let next_pg;
        {
            let old = self.page(parent_pgno);
            let mut l = PageBufMut::new(&mut l_buf);
            let mut r = PageBufMut::new(&mut r_buf);
            l.set_prev_pg(old.prev_pg());
            l.set_next_pg(r_pgno);
            r.set_prev_pg(parent_pgno);
            next_pg = old.next_pg();
            r.set_next_pg(next_pg);

            let view = PageView::parse(parent_pgno, old.bytes(), false)?;
            let n = view.nentries();
            let plan = compute_psplit_split_internal(&view, skip, ilen, psize)?;

            copy_internal_entries(&view, 0..plan.h_in_l(), plan.l_skip(skip), &mut l)?;
            copy_internal_entries(&view, plan.h_in_l()..n, plan.r_skip(skip), &mut r)?;

            let target = if plan.right_target { &mut r } else { &mut l };
            write_internal_entry_at_slot(
                target,
                plan.target_slot(skip),
                nksize,
                sep.sep_key(),
                sep.child(),
            );
        }

        let r_first_key = first_internal_key(&r_buf)?;

        // Phase 2: infallible commit.
        self.replace_page(parent_pgno, &l_buf);
        let pushed = self.push_page(&r_buf);
        debug_assert_eq!(pushed, r_pgno);

        if next_pg != P_INVALID {
            self.page_mut(next_pg).set_prev_pg(r_pgno);
        }

        // The separator propagating up to the grandparent is r's
        // first internal entry, used verbatim with no further prefix
        // compression (matches the P_BINTERNAL arm of __bt_split's
        // parent-insert loop, which copies the entry as-is).
        let ksize = r_first_key.len();
        Ok(Separator::Internal {
            sep_key: r_first_key,
            child: r_pgno,
            ksize,
        })
    }

    /// Split the root leaf at insertion index `skip`, then insert the
    /// new `(key, val)` entry into whichever half it belongs in.
    /// Afterwards the original root page (page 1) is converted into a
    /// btree internal page with two children, matching the
    /// `bt_root` + `bt_psplit` + `bt_broot` path in `bt_split.c`.
    fn split_root_leaf(&mut self, skip: usize, key: &[u8], val: &[u8]) -> Result<()> {
        let psize = self.psize;
        let ilen = align_entry(BLEAF_HEADER_SIZE + key.len() + val.len());

        // Predict l_pgno, r_pgno; the arena is not actually grown
        // until phase 2 completes.  `bt_root` links the two new
        // leaves as a sibling pair with the left at the head of the
        // level (`prevpg == P_INVALID`) and the right at the tail.
        let l_pgno = self.next_free_pgno();
        let r_pgno = l_pgno + 1;

        let mut l_buf = empty_leaf(l_pgno, psize);
        let mut r_buf = empty_leaf(r_pgno, psize);
        let r_first_key = {
            let mut l = PageBufMut::new(&mut l_buf);
            let mut r = PageBufMut::new(&mut r_buf);
            l.set_next_pg(r_pgno);
            r.set_prev_pg(l_pgno);

            let old_root = self.page(P_ROOT);
            let view = PageView::parse(P_ROOT, old_root.bytes(), false)?;
            let n = view.nentries();
            let plan = compute_psplit_split_leaf(&view, skip, ilen, psize)?;
            copy_entries(&view, 0..plan.h_in_l(), plan.l_skip(skip), &mut l)?;
            copy_entries(&view, plan.h_in_l()..n, plan.r_skip(skip), &mut r)?;

            let target = if plan.right_target { &mut r } else { &mut l };
            write_entry_at_slot(target, plan.target_slot(skip), key, val);

            // Pull the right page's first key out before we hand
            // r back to the page map; the new internal root's
            // separator (`linp[1]`) holds a copy of it.
            leaf_first_key(r.bytes())?
        };

        // Phase 2: build the new internal root in a scratch buffer
        // (a clone of the old root preserves `bt_broot`'s
        // leave-`linp[2..]`-untouched behaviour).  The
        // `convert_root_to_internal` step is the last fallible
        // operation; on Err the arena is still untouched, so a
        // separator that turns out to be too large to fit a fresh
        // internal root cleanly aborts the put.
        let mut new_root_buf = self.page_bytes(P_ROOT).to_vec();
        convert_root_to_internal(
            &mut PageBufMut::new(&mut new_root_buf),
            psize,
            l_pgno,
            r_pgno,
            &r_first_key,
        );

        let l_pushed = self.push_page(&l_buf);
        debug_assert_eq!(l_pushed, l_pgno);
        let r_pushed = self.push_page(&r_buf);
        debug_assert_eq!(r_pushed, r_pgno);
        self.replace_page(P_ROOT, &new_root_buf);

        Ok(())
    }

    /// Flush all buffered pages to disk, consuming the writer.
    ///
    /// Pages are written in pgno order through a single buffered
    /// writer (no per-page seeks): the in-memory arena is one
    /// contiguous `psize`-aligned buffer indexed by pgno, so finish
    /// writes the meta page into slot 0 and then flushes the whole
    /// buffer as a single `write_all`.
    ///
    /// No `fsync` is issued: this matches libnbcompat's `dbopen`,
    /// where the close path just unmaps and flushes userspace
    /// buffers.  Callers that need on-disk durability should `fsync`
    /// the containing directory themselves.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if any of the writes fail.
    ///
    /// # Panics
    ///
    /// Panics if the arena is not a positive whole multiple of
    /// `psize` bytes.  This is a writer-internal invariant
    /// established at construction; a failure here indicates a
    /// bug in this crate, and asserting rather than silently
    /// truncating is the only safe choice at the persistence
    /// boundary.
    ///
    /// [`Error::Io`]: crate::Error::Io
    pub fn finish(self) -> Result<()> {
        let Self {
            file,
            psize,
            mut pages,
            free,
            nrecs,
            flags,
        } = self;

        assert!(
            !pages.is_empty() && pages.len() % psize == 0,
            "arena length {len} is not a positive multiple of psize {psize}",
            len = pages.len(),
        );

        pages[..psize].copy_from_slice(&build_meta(psize, free, nrecs, flags));
        let mut bw = BufWriter::with_capacity(64 * 1024, file);
        bw.write_all(&pages)?;
        bw.flush()?;
        Ok(())
    }
}

/// Build a fresh meta page.  Layout follows `BTMETA` in
/// `nbcompat/db.h`: magic, version, psize, free, nrecs, flags, then
/// `psize - 24` zero bytes.
fn build_meta(psize: usize, free: u32, nrecs: u32, flags: u32) -> Vec<u8> {
    let mut p = vec![0u8; psize];
    write_u32(&mut p, 0, BTREE_MAGIC);
    write_u32(&mut p, 4, BTREE_VERSION);
    // `psize` is bounded by [MIN_PSIZE, MAX_PSIZE], so this cast is
    // lossless.  Same justification used throughout this file.
    write_u32(&mut p, 8, psize_as_u32(psize));
    write_u32(&mut p, 12, free);
    write_u32(&mut p, 16, nrecs);
    write_u32(&mut p, 20, flags);
    p
}

/// Build an empty leaf page at `pgno` of `psize` bytes.  Header is
/// filled in; the remainder is left zero.
fn empty_leaf(pgno: u32, psize: usize) -> Vec<u8> {
    empty_page(pgno, psize, P_BLEAF)
}

/// Build an empty internal page at `pgno` of `psize` bytes.
fn empty_internal(pgno: u32, psize: usize) -> Vec<u8> {
    empty_page(pgno, psize, P_BINTERNAL)
}

fn empty_page(pgno: u32, psize: usize, flags: u32) -> Vec<u8> {
    let mut buf = vec![0u8; psize];
    let mut p = PageBufMut::new(&mut buf);
    p.set_pgno(pgno);
    p.set_flags(flags);
    p.set_lower(PAGE_HEADER_SIZE);
    p.set_upper(psize);
    buf
}

/// Convert a page size to `u32` for the meta page's `psize` field.
/// `psize` is bounded by `MAX_PSIZE = 32768`, so the conversion is
/// lossless.
#[inline]
fn psize_as_u32(psize: usize) -> u32 {
    debug_assert!(psize <= MAX_PSIZE);
    u32::try_from(psize).expect("psize <= MAX_PSIZE")
}

/// Number of bytes of `b` needed to be strictly greater than `a`,
/// assuming `a < b`.  Mirrors `__bt_defpfx` in `bt_utils.c`:
/// returns the 1-indexed position of the first differing byte; if
/// `a` is a proper prefix of `b`, returns `a.len() + 1`.  The
/// fallthrough on `a.len() >= b.len()` shouldn't be reachable
/// under the `a < b` precondition, but is preserved for parity
/// with the C function.
fn bt_defpfx(a: &[u8], b: &[u8]) -> usize {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(i) => i + 1,
        None if a.len() < b.len() => a.len() + 1,
        None => a.len(),
    }
}

/// Walk h's entries left-to-right summing byte cost (entry plus a
/// `linp` slot) until the running total reaches half the page's
/// usable space.  Returns a [`SplitPlan`] describing how many
/// entries go to each side and whether the new entry lands in the
/// right page.  Matches `bt_psplit`'s first loop for the BLEAF case
/// without overflow entries.
fn compute_psplit_split_leaf(
    view: &PageView<'_>,
    skip: usize,
    ilen: usize,
    psize: usize,
) -> Result<SplitPlan> {
    psplit_loop(skip, ilen, psize, view.nentries(), |idx| {
        entry_nbytes(view, idx)
    })
}

/// Decoded bytes of a single leaf entry on `view` at index `idx`,
/// inline only.  Used during a split to compute byte costs.
fn entry_nbytes(view: &PageView<'_>, idx: usize) -> Result<usize> {
    let entry = view.leaf_entry(idx)?;
    let key = view.inline(entry.key)?;
    let val = view.inline(entry.data)?;
    Ok(align_entry(BLEAF_HEADER_SIZE + key.len() + val.len()))
}

/// Copy entries `view[range]` into the destination page in order.
/// If `reserve_at` is `Some(slot)`, that destination slot is left
/// blank for the caller to fill with the new entry later.  Used for
/// both halves of a leaf split: `Some(skip)` on whichever side will
/// own the new entry, `None` on the other.
fn copy_entries(
    view: &PageView<'_>,
    range: std::ops::Range<usize>,
    reserve_at: Option<usize>,
    dst: &mut PageBufMut<'_>,
) -> Result<()> {
    let mut slot = 0usize;
    for src_idx in range {
        if Some(slot) == reserve_at {
            slot += 1;
        }
        copy_one(view, src_idx, slot, dst)?;
        slot += 1;
    }
    if Some(slot) == reserve_at {
        slot += 1;
    }
    dst.set_lower(PAGE_HEADER_SIZE + slot * 2);
    Ok(())
}

/// Copy one inline leaf entry from `view[src_idx]` onto `dst` at the
/// next available downward offset, writing `dst.linp[dst_slot]` to
/// point at it.  Mirrors the per-entry work of `bt_psplit`'s inner
/// loop.
fn copy_one(
    view: &PageView<'_>,
    src_idx: usize,
    dst_slot: usize,
    dst: &mut PageBufMut<'_>,
) -> Result<()> {
    let entry = view.leaf_entry(src_idx)?;
    let key = view.inline(entry.key)?;
    let val = view.inline(entry.data)?;
    let nbytes = align_entry(BLEAF_HEADER_SIZE + key.len() + val.len());

    let new_upper = dst.upper() - nbytes;
    dst.set_upper(new_upper);
    dst.set_linp(dst_slot, new_upper);
    write_bleaf_inline(dst.bytes_mut(), new_upper, key, val);
    Ok(())
}

/// Remove the leaf entry at index `idx` from `page`, compacting the
/// page in place so the byte layout matches what `__bt_dleaf` in
/// `bt_delete.c` produces.  All entries previously placed *after*
/// the deleted one (smaller offset) shift right by the deleted
/// entry's `nbytes`; their `linp` values get adjusted.
fn delete_leaf_entry(page: &mut PageBufMut<'_>, idx: usize) {
    let lower = page.lower();
    let upper = page.upper();
    let n = page.nentries();
    let offset = page.linp(idx) as usize;
    let ksize = read_u32_at(page.bytes(), offset) as usize;
    let dsize = read_u32_at(page.bytes(), offset + 4) as usize;
    let nbytes = align_entry(BLEAF_HEADER_SIZE + ksize + dsize);

    // Shift entry payloads: everything in `[upper, offset)` moves
    // right by `nbytes`, overwriting the deleted entry.
    page.bytes_mut().copy_within(upper..offset, upper + nbytes);

    // Adjust linp[0..idx]: any pointer below `offset` shifts right.
    for i in 0..idx {
        let v = page.linp(i) as usize;
        if v < offset {
            page.set_linp(i, v + nbytes);
        }
    }
    // Adjust linp[idx..n-1]: shift left by one and adjust the same
    // way.  linp[n-1] becomes free.
    for i in idx..n - 1 {
        let v = page.linp(i + 1) as usize;
        let new = if v < offset { v + nbytes } else { v };
        page.set_linp(i, new);
    }

    page.set_lower(lower - 2);
    page.set_upper(upper + nbytes);
}

/// Place the new leaf entry into the reserved `linp[slot]` slot on
/// `dst`.  After this, `dst` is fully populated.
fn write_entry_at_slot(dst: &mut PageBufMut<'_>, slot: usize, key: &[u8], val: &[u8]) {
    let nbytes = align_entry(BLEAF_HEADER_SIZE + key.len() + val.len());
    let new_upper = dst.upper() - nbytes;
    dst.set_upper(new_upper);
    dst.set_linp(slot, new_upper);
    write_bleaf_inline(dst.bytes_mut(), new_upper, key, val);
}

/// Return the inline key of the first leaf entry on `page` as an
/// owned `Vec<u8>`.  Used to compute the right-sibling's separator
/// after a leaf split.
fn leaf_first_key(page: &[u8]) -> Result<Vec<u8>> {
    let view = PageView::parse(0, page, false)?;
    Ok(view.inline(view.leaf_entry(0)?.key)?.to_vec())
}

/// Return the inline key of the last leaf entry on `page` as an
/// owned `Vec<u8>`.  Used as the prefix-compression input for the
/// separator going to the parent.
fn leaf_last_key(page: &[u8]) -> Result<Vec<u8>> {
    let view = PageView::parse(0, page, false)?;
    let last = view.nentries() - 1;
    Ok(view.inline(view.leaf_entry(last)?.key)?.to_vec())
}

/// Convert the original root page at `p` into a 2-entry internal
/// page pointing at `l_pgno` (zero-key separator at `linp[0]`,
/// matching the leftmost-spine convention) and `r_pgno` (key =
/// `r_first_key` at `linp[1]`).  Mirrors `bt_broot`'s in-place
/// modification: only the header fields, `linp[0..2]`, and the
/// two new internal entries at the page tail are touched -
/// everything else (the original page's `linp[2..]` and entry
/// data) is left as it was, matching the C side byte for byte.
///
/// Used for both the leaf->internal root promotion (after the
/// root leaf splits for the first time) and the internal->internal
/// root replacement (after a deeper root split): the type-bit
/// rewrite below is idempotent for an already-internal page.
///
/// `r_first_key` is guaranteed to fit by [`Writer::put`]'s
/// separator-bound pre-check: every key that ever enters the tree
/// is small enough to serve as a root separator alongside the
/// 12-byte zero-key entry and two `linp` slots on a fresh page.
fn convert_root_to_internal(
    p: &mut PageBufMut<'_>,
    psize: usize,
    l_pgno: u32,
    r_pgno: u32,
    r_first_key: &[u8],
) {
    let upper0 = psize - align_entry(BINTERNAL_HEADER_SIZE);
    let upper1 = upper0 - align_entry(BINTERNAL_HEADER_SIZE + r_first_key.len());
    let bytes = p.bytes_mut();
    write_binternal_inline(bytes, upper0, 0, &[], l_pgno);
    write_binternal_inline(bytes, upper1, r_first_key.len(), r_first_key, r_pgno);

    p.set_linp(0, upper0);
    p.set_linp(1, upper1);
    p.set_lower(PAGE_HEADER_SIZE + 4);
    p.set_upper(upper1);
    let new_flags = (p.flags() & !P_TYPE) | P_BINTERNAL;
    p.set_flags(new_flags);
}

// =========================================================================
// Spine / Separator types used by the descent and split-propagation path.
// =========================================================================

/// Path from the root to a leaf, as recorded during descent.  Each
/// element is `(internal_page_pgno, linp_slot_descended_from)`; the
/// leaf is named separately.  Pop the path to walk back up the tree
/// when a leaf split has to be propagated through parents.
struct Spine {
    path: Vec<(u32, usize)>,
    leaf_pgno: u32,
}

/// What needs to be inserted into a parent internal page after a
/// child page below has split.
///
/// Mirrors the two arms of `__bt_split`'s parent-insert loop:
/// leaf-source separators (built from a leaf's first key, where
/// the parent insert can prefix-compress against the left leaf's
/// last key) and internal-source separators (copied verbatim from
/// the new right sibling's first `BINTERNAL` entry, already
/// prefix-compressed when it was created).
enum Separator {
    /// Built from a leaf split.  `sep_key` is the first key of the
    /// new right leaf; `l_last_key` is the last key of the left
    /// leaf and drives prefix compression at the parent insert.
    Leaf {
        sep_key: Vec<u8>,
        child: u32,
        l_last_key: Vec<u8>,
    },
    /// Built from an internal-page split.  `sep_key` is the new
    /// right sibling's first `BINTERNAL` entry key (already
    /// prefix-compressed at creation time); `ksize` is the original
    /// stored ksize of that entry, which is preserved verbatim.
    Internal {
        sep_key: Vec<u8>,
        child: u32,
        ksize: usize,
    },
}

impl Separator {
    /// The new entry's child pgno.
    const fn child(&self) -> u32 {
        match self {
            Self::Leaf { child, .. } | Self::Internal { child, .. } => *child,
        }
    }

    /// The new entry's key bytes (full length, before any
    /// truncation to a prefix-compressed `ksize`).
    fn sep_key(&self) -> &[u8] {
        match self {
            Self::Leaf { sep_key, .. } | Self::Internal { sep_key, .. } => sep_key,
        }
    }

    /// Encoded `(nksize, nbytes)` for inserting this separator into
    /// a parent whose `prev_pg` and target slot are given.
    /// Internal-source separators use their stored fixed ksize;
    /// leaf-source separators try prefix compression against the
    /// left leaf's last key when the slot allows it.
    fn encoded_size(&self, parent_prev_pg: u32, skip: usize) -> (usize, usize) {
        match self {
            Self::Internal { ksize, .. } => (*ksize, align_entry(BINTERNAL_HEADER_SIZE + ksize)),
            Self::Leaf {
                sep_key,
                l_last_key,
                ..
            } => {
                let full = sep_key.len();
                let full_nbytes = align_entry(BINTERNAL_HEADER_SIZE + full);
                let allow_pfx = parent_prev_pg != P_INVALID || skip > 1;
                if !allow_pfx {
                    return (full, full_nbytes);
                }
                let pfx_len = bt_defpfx(l_last_key, sep_key);
                let compressed = align_entry(BINTERNAL_HEADER_SIZE + pfx_len);
                if compressed < full_nbytes {
                    (pfx_len, compressed)
                } else {
                    (full, full_nbytes)
                }
            }
        }
    }
}

// =========================================================================
// Internal-page helpers (descent, psplit, copy, etc.).
// =========================================================================

/// Choose the linp slot to follow when descending through an internal
/// page.  Mirrors `__bt_search`'s binary search with the same
/// leftmost-spine special case.
fn descend_internal_pick(view: &PageView<'_>, key: &[u8], leftmost: bool) -> Result<usize> {
    let n = view.nentries();
    let mut base = 0usize;
    let mut lim = n;
    let mut matched: Option<usize> = None;
    while lim > 0 {
        let mid = base + (lim >> 1);
        let entry = view.internal_entry(mid)?;
        let cmp = if mid == 0 && leftmost {
            Ordering::Greater
        } else {
            key.cmp(view.inline(entry.key)?)
        };
        match cmp {
            Ordering::Equal => {
                matched = Some(mid);
                break;
            }
            Ordering::Greater => {
                base = mid + 1;
                lim -= 1;
            }
            Ordering::Less => {}
        }
        lim >>= 1;
    }
    Ok(match matched {
        Some(idx) => idx,
        None if base == 0 => 0,
        None => base - 1,
    })
}

/// `bt_psplit` for an internal page: same shape as the leaf variant
/// but uses `BINTERNAL` entry sizes.
fn compute_psplit_split_internal(
    view: &PageView<'_>,
    skip: usize,
    ilen: usize,
    psize: usize,
) -> Result<SplitPlan> {
    psplit_loop(skip, ilen, psize, view.nentries(), |idx| {
        internal_entry_nbytes(view, idx)
    })
}

/// Decision returned by the byte-balanced split loop.
#[derive(Clone, Copy, Debug)]
struct SplitPlan {
    /// Number of slots in the left page after the split, including
    /// the reserved slot for the new entry when it lands on the left.
    left_slots: usize,
    /// `true` if the new entry should go to the right page.
    right_target: bool,
}

impl SplitPlan {
    /// Number of source entries copied to the left page (= slot
    /// count minus 1 if the new entry is reserved on the left).
    const fn h_in_l(self) -> usize {
        if self.right_target {
            self.left_slots
        } else {
            self.left_slots - 1
        }
    }

    /// Slot index in the left page where the new entry is reserved,
    /// or `None` if it landed on the right.
    const fn l_skip(self, skip: usize) -> Option<usize> {
        if self.right_target { None } else { Some(skip) }
    }

    /// Slot index in the right page where the new entry is reserved,
    /// or `None` if it landed on the left.
    const fn r_skip(self, skip: usize) -> Option<usize> {
        if self.right_target {
            Some(skip - self.left_slots)
        } else {
            None
        }
    }

    /// The destination linp slot index of the new entry in whichever
    /// page receives it.
    const fn target_slot(self, skip: usize) -> usize {
        if self.right_target {
            skip - self.left_slots
        } else {
            skip
        }
    }
}

/// The byte-balanced inner loop shared between the leaf and internal
/// split paths.  `entry_size(idx)` returns the encoded byte cost of
/// `h[idx]`.
fn psplit_loop(
    skip: usize,
    ilen: usize,
    psize: usize,
    top: usize,
    mut entry_size: impl FnMut(usize) -> Result<usize>,
) -> Result<SplitPlan> {
    let full = psize - PAGE_HEADER_SIZE;
    let half = full / 2;
    let mut used: usize = 0;
    let mut nxt = 0usize;
    let mut off = 0usize;
    loop {
        if nxt >= top {
            break;
        }
        let nbytes = if skip == off { ilen } else { entry_size(nxt)? };
        // Same early break as C `bt_psplit`: stop if placing this
        // entry would overflow the page with the new entry on the
        // same side, or if we'd be handing the last source entry to
        // the left page (psplit always leaves at least one entry for
        // the right side).
        if (skip <= off && used + nbytes + 2 >= full) || nxt + 1 == top {
            break;
        }
        if skip != off {
            nxt += 1;
        }
        used += nbytes + 2;
        off += 1;
        if used >= half {
            break;
        }
    }
    // After the loop, `off` is the left page's slot count
    // (including the reserved skip slot if the new entry landed
    // left).  Matches C's `off + 1` post-`--off; break` for early
    // breaks and `off + 1` post-`break` for the half-fill break.
    let left_slots = off;
    let right_target = skip >= left_slots;
    Ok(SplitPlan {
        left_slots,
        right_target,
    })
}

fn internal_entry_nbytes(view: &PageView<'_>, idx: usize) -> Result<usize> {
    let entry = view.internal_entry(idx)?;
    let key = view.inline(entry.key)?;
    Ok(align_entry(BINTERNAL_HEADER_SIZE + key.len()))
}

/// Internal-page analogue of [`copy_entries`].
fn copy_internal_entries(
    view: &PageView<'_>,
    range: std::ops::Range<usize>,
    reserve_at: Option<usize>,
    dst: &mut PageBufMut<'_>,
) -> Result<()> {
    let mut slot = 0usize;
    for src_idx in range {
        if Some(slot) == reserve_at {
            slot += 1;
        }
        copy_internal_one(view, src_idx, slot, dst)?;
        slot += 1;
    }
    if Some(slot) == reserve_at {
        slot += 1;
    }
    dst.set_lower(PAGE_HEADER_SIZE + slot * 2);
    Ok(())
}

fn copy_internal_one(
    view: &PageView<'_>,
    src_idx: usize,
    dst_slot: usize,
    dst: &mut PageBufMut<'_>,
) -> Result<()> {
    let entry = view.internal_entry(src_idx)?;
    let key = view.inline(entry.key)?;
    let nbytes = align_entry(BINTERNAL_HEADER_SIZE + key.len());

    let new_upper = dst.upper() - nbytes;
    dst.set_upper(new_upper);
    dst.set_linp(dst_slot, new_upper);
    write_binternal_inline(dst.bytes_mut(), new_upper, key.len(), key, entry.child);
    Ok(())
}

/// Place a new internal entry at `slot` on a fresh split page.
/// The page must already have `upper` set to where entries are
/// being placed (initialised either to `psize` for empty pages or
/// to the trailing value left by previous `copy_internal_one`
/// calls).
fn write_internal_entry_at_slot(
    dst: &mut PageBufMut<'_>,
    slot: usize,
    nksize: usize,
    sep_key: &[u8],
    child: u32,
) {
    let nbytes = align_entry(BINTERNAL_HEADER_SIZE + nksize);
    let new_upper = dst.upper() - nbytes;
    dst.set_upper(new_upper);
    dst.set_linp(slot, new_upper);
    write_binternal_inline(dst.bytes_mut(), new_upper, nksize, sep_key, child);
}

/// Insert a new internal entry into an in-room internal page at
/// linp slot `skip`.  Updates `linp[..]`, `lower`, `upper`.
fn insert_internal_in_place(
    page: &mut PageBufMut<'_>,
    skip: usize,
    nksize: usize,
    sep_key: &[u8],
    child: u32,
) {
    let nbytes = align_entry(BINTERNAL_HEADER_SIZE + nksize);
    let new_upper = page.reserve_slot(skip, nbytes);
    write_binternal_inline(page.bytes_mut(), new_upper, nksize, sep_key, child);
}

/// Copy the first internal entry's inline key from a fully-built
/// internal page, returning an owned `Vec<u8>` for use as the
/// `linp[1]` key on the new internal root.  The original `ksize`
/// of the source entry is exactly `key.len()`.
fn first_internal_key(page: &[u8]) -> Result<Vec<u8>> {
    let view = PageView::parse(0, page, false)?;
    let entry = view.internal_entry(0)?;
    Ok(view.inline(entry.key)?.to_vec())
}

/// Binary-search the leaf at `pgno` for `key`.  Returns
/// `(exact, idx)`: `exact` is `true` if a record with `key` exists,
/// in which case `idx` is its position; otherwise `idx` is the
/// position where the new record should be inserted.
///
/// `pgno` is passed only so that any [`PageView`] parsing error
/// reports the correct page number in [`Error::CorruptPage`];
/// successful operation doesn't depend on it.  The pages we write
/// only ever contain inline leaf entries, so `view.inline(...)`
/// always succeeds; an overflow ref here would be a bug in
/// [`Writer`] itself.
///
/// [`Error::CorruptPage`]: crate::Error::CorruptPage
fn leaf_search(pgno: u32, page: PageBufRef<'_>, key: &[u8]) -> Result<(bool, usize)> {
    let view = PageView::parse(pgno, page.bytes(), false)?;
    let mut base = 0usize;
    let mut lim = view.nentries();
    while lim > 0 {
        let idx = base + (lim >> 1);
        let entry = view.leaf_entry(idx)?;
        let entry_key = view.inline(entry.key)?;
        match key.cmp(entry_key) {
            Ordering::Equal => return Ok((true, idx)),
            Ordering::Greater => {
                base = idx + 1;
                lim -= 1;
            }
            Ordering::Less => {}
        }
        lim >>= 1;
    }
    Ok((false, base))
}

/// Insert a new leaf entry at slot `idx`, splicing it into
/// `linp[]` and writing the BLEAF header + payload into the free
/// region of the page.  Caller must have already verified
/// `page.free_space() >= align(9 + key.len() + val.len()) + 2`
/// (see `Writer::put` for the actual rejection path on too-large
/// entries).
fn insert_leaf_entry(page: &mut PageBufMut<'_>, idx: usize, key: &[u8], val: &[u8]) {
    let nbytes = align_entry(BLEAF_HEADER_SIZE + key.len() + val.len());
    let new_upper = page.reserve_slot(idx, nbytes);
    write_bleaf_inline(page.bytes_mut(), new_upper, key, val);
}

/// Convert an entry's key or value length to `u32`.  Bounded by the
/// page size, so the conversion is lossless.
#[inline]
fn len_as_u32(len: usize) -> u32 {
    debug_assert!(len <= MAX_PSIZE);
    u32::try_from(len).expect("len <= MAX_PSIZE")
}

#[inline]
fn write_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_ne_bytes());
}

#[inline]
fn read_u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().expect("4 bytes"))
}

/// Write a `BLEAF` inline entry into `bytes` starting at offset
/// `off`: header (`ksize`, `dsize`, `flags=0`) followed by the key
/// then the value.  Padding bytes between the entry's last written
/// byte (`off + 9 + key.len() + val.len()`) and the next 4-byte
/// boundary are left untouched, matching `libnbcompat`'s
/// `WR_BLEAF`.  Inline-only: the `P_BIGKEY` / `P_BIGDATA` flags
/// are never set by this writer.
fn write_bleaf_inline(bytes: &mut [u8], off: usize, key: &[u8], val: &[u8]) {
    write_u32(bytes, off, len_as_u32(key.len()));
    write_u32(bytes, off + 4, len_as_u32(val.len()));
    bytes[off + 8] = 0;
    let key_pos = off + BLEAF_HEADER_SIZE;
    bytes[key_pos..key_pos + key.len()].copy_from_slice(key);
    let val_pos = key_pos + key.len();
    bytes[val_pos..val_pos + val.len()].copy_from_slice(val);
}

/// Write a `BINTERNAL` inline entry into `bytes` starting at
/// offset `off`: header (`ksize`, `child`, `flags=0`) followed by
/// the first `ksize` bytes of `key`.  `key.len()` is allowed to be
/// `>= ksize` (the high bytes are dropped); this is how
/// prefix-compressed separators get written without first
/// truncating the key.
fn write_binternal_inline(bytes: &mut [u8], off: usize, ksize: usize, key: &[u8], child: u32) {
    debug_assert!(key.len() >= ksize);
    write_u32(bytes, off, len_as_u32(ksize));
    write_u32(bytes, off + 4, child);
    bytes[off + 8] = 0;
    let key_pos = off + BINTERNAL_HEADER_SIZE;
    bytes[key_pos..key_pos + ksize].copy_from_slice(&key[..ksize]);
}
