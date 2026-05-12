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
 * Read and write Berkeley DB 1.85 btree files - the on-disk format
 * used by 4.4BSD's `dbopen(3)` and still shipped today in NetBSD
 * `libc` and pkgsrc's `libnbcompat`.  Hash and recno access methods
 * are out of scope.
 *
 * # Reader
 *
 * [`Db`] opens an existing file, retrieves values by key, and
 * iterates all key/value pairs in sorted order via [`Iter`].  Files
 * written in either byte order are accepted.
 *
 * # Writer
 *
 * [`Writer`] builds a new file from scratch via a sequence of
 * [`Writer::put`] / [`Writer::del`] operations, flushed on
 * [`Writer::finish`].  It does not modify existing files, and writes
 * native-endian only (the reader handles both byte orders).
 *
 * Each key/value pair must fit in a single page (~4 KiB); larger
 * entries return [`Error::EntryTooLarge`].  This is enough to
 * rebuild pkgsrc's `pkgdb.byfile.db`, but means this is not a full
 * Berkeley DB 1.85 writer.
 *
 * [`Error::EntryTooLarge`]: crate::Error::EntryTooLarge
 *
 * # Errors
 *
 * Fallible operations return [`Result<T>`].  [`Error`] distinguishes
 * I/O failures from on-disk format problems (bad magic, unsupported
 * version, corrupt pages).
 *
 * # Example
 *
 * ```no_run
 * use db185::Db;
 *
 * # fn run() -> db185::Result<()> {
 * let db = Db::open("/var/db/pkg/pkgdb.byfile.db")?;
 * if let Some(value) = db.get(b"/opt/pkg/bin/foo\0")? {
 *     println!("{}", String::from_utf8_lossy(value.as_ref()));
 * }
 * for entry in &db {
 *     let entry = entry?;
 *     println!(
 *         "{} -> {}",
 *         String::from_utf8_lossy(entry.key()),
 *         String::from_utf8_lossy(entry.value()),
 *     );
 * }
 * # Ok(())
 * # }
 * ```
 */

#![warn(clippy::pedantic, clippy::nursery, missing_docs, unreachable_pub)]
// `unreachable_pub` wants `pub(crate)` on items inside private modules
// whose visibility is only used within the crate.  That conflicts with
// `clippy::redundant_pub_crate` (nursery), which considers the same
// `pub(crate)` redundant since the module is already private.  Rustc's
// view is the more informative one, so suppress the clippy lint.
#![allow(clippy::redundant_pub_crate)]

mod db;
mod error;
mod format;
mod iter;
mod page;
mod writer;

pub use db::Db;
pub use error::Error;
pub use iter::{Entry, Iter};
pub use writer::Writer;

/// Result alias for fallible operations in this crate.
pub type Result<T> = std::result::Result<T, Error>;
