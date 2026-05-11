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
 * Forward sequential scan.
 *
 * Equivalent to `seq(R_FIRST)` followed by repeated `seq(R_NEXT)` in
 * the historical `dbopen(3)` API.  Iteration starts at the leftmost
 * leaf (found by descending child 0 of every internal page) and
 * advances along the leaf sibling chain via the `nextpg` link.
 *
 * The iterator yields [`Entry`] values that borrow directly from the
 * database's `mmap`-backed pages: an inline key or value is returned
 * as a `Cow::Borrowed(&[u8])` with no per-entry allocation.  Overflow
 * keys and values are materialised at yield time into `Cow::Owned`.
 */

use crate::Result;
use crate::db::Db;
use crate::error::Error;
use crate::format::{P_INVALID, P_ROOT};
use crate::page::PageView;
use std::borrow::Cow;
use std::iter::FusedIterator;

/// A key/value pair yielded by [`Iter`].
///
/// `Entry` borrows from the database's `mmap`; its slices are valid
/// for the lifetime of the borrowed [`Db`].  Inline fields cost no
/// allocation; overflow fields hold an owned `Vec<u8>`.
#[derive(Clone, Debug)]
pub struct Entry<'a> {
    key: Cow<'a, [u8]>,
    value: Cow<'a, [u8]>,
}

impl<'a> Entry<'a> {
    /// Borrow the key bytes for this entry.
    #[inline]
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Borrow the value bytes for this entry.
    #[inline]
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// Decompose into key and value.  Inline fields are returned as
    /// `Cow::Borrowed` slices into the database's mmap; overflowed
    /// fields are returned as `Cow::Owned`.
    #[must_use]
    pub fn into_parts(self) -> (Cow<'a, [u8]>, Cow<'a, [u8]>) {
        (self.key, self.value)
    }
}

/// Forward iterator over a [`Db`].  Yields `Result<Entry>`; iteration
/// stops on the first error.
pub struct Iter<'a> {
    db: &'a Db,
    state: State<'a>,
}

/// Where the iterator is currently positioned.
enum State<'a> {
    /// Not yet initialised; the tree spine has not been descended.
    Pristine,
    /// Positioned on a loaded leaf page.
    Active(Cursor<'a>),
    /// Iterator exhausted or terminated by an error.
    Done,
}

/// A parsed leaf page plus the index of the next entry to yield.
///
/// Caching the [`PageView`] inside the cursor means the page header
/// is byteswap-decoded exactly once per leaf, no matter how many
/// entries the leaf contains.
struct Cursor<'a> {
    view: PageView<'a>,
    index: usize,
}

impl<'a> Iter<'a> {
    pub(crate) const fn new(db: &'a Db) -> Self {
        Self {
            db,
            state: State::Pristine,
        }
    }

    /// Walk down the left spine of the tree to the leftmost non-empty
    /// leaf.  Returns [`State::Done`] for an empty tree.
    fn initial_state(&self) -> Result<State<'a>> {
        let mut pgno = P_ROOT;
        loop {
            let view = PageView::parse(pgno, self.db.page(pgno)?, self.db.swap())?;
            if view.nentries() == 0 {
                return Ok(State::Done);
            }
            if view.is_leaf() {
                return Ok(State::Active(Cursor { view, index: 0 }));
            }
            if !view.is_internal() {
                return Err(Error::CorruptPage {
                    pgno,
                    reason: "non-internal page during seq descent",
                });
            }
            pgno = view.internal_entry(0)?.child;
        }
    }

    /// Move to the start of the next non-empty leaf, or to
    /// [`State::Done`] when no more siblings remain.
    fn advance_leaf(&self, mut next_pg: u32) -> Result<State<'a>> {
        while next_pg != P_INVALID {
            let view = PageView::parse(next_pg, self.db.page(next_pg)?, self.db.swap())?;
            if !view.is_leaf() {
                return Err(Error::CorruptPage {
                    pgno: next_pg,
                    reason: "seq sibling is not a leaf",
                });
            }
            if view.nentries() > 0 {
                return Ok(State::Active(Cursor { view, index: 0 }));
            }
            next_pg = view.next_pg;
        }
        Ok(State::Done)
    }

    /// Decode the entry at `cursor`, advance the iterator state, and
    /// return the resulting [`Entry`].
    fn yield_current(&mut self, cursor: Cursor<'a>) -> Result<Entry<'a>> {
        let leaf = cursor.view.leaf_entry(cursor.index)?;
        let key = self.db.materialise(&cursor.view, leaf.key)?;
        let value = self.db.materialise(&cursor.view, leaf.data)?;

        self.state = if cursor.index + 1 < cursor.view.nentries() {
            State::Active(Cursor {
                view: cursor.view,
                index: cursor.index + 1,
            })
        } else {
            self.advance_leaf(cursor.view.next_pg)?
        };

        Ok(Entry { key, value })
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = Result<Entry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match std::mem::replace(&mut self.state, State::Done) {
                State::Done => return None,
                State::Pristine => match self.initial_state() {
                    Ok(state) => self.state = state,
                    Err(e) => return Some(Err(e)),
                },
                State::Active(cursor) => return Some(self.yield_current(cursor)),
            }
        }
    }
}

impl FusedIterator for Iter<'_> {}
