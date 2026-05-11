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
 * Read access to BSD 4.4 `dbopen(3)` btree (db 1.85) files.
 *
 * This crate implements the on-disk btree format used by the historical
 * 4.4BSD `dbopen(3)` interface, as preserved in NetBSD `libc` and in the
 * pkgsrc `libnbcompat` sources.  It targets the subset required by
 * `pkg_install`'s `pkgdb.byfile.db`: btree only, no duplicates, default
 * `memcmp` ordering, no user comparison or prefix callbacks.  Hash and
 * recno access methods are not supported.
 *
 * Only read operations are implemented in this revision: opening an
 * existing database, retrieving a value by key, and iterating all
 * key/value pairs in sorted order.  Write support will follow.
 *
 * # Errors
 *
 * Fallible operations return [`Result<T>`], aliased to
 * [`std::result::Result`] over [`Error`].  [`Error`] distinguishes
 * underlying I/O failures from on-disk format problems (bad magic,
 * unsupported version, corrupt pages or overflow chains).
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

pub use db::Db;
pub use error::Error;
pub use iter::{Entry, Iter};

/// Result alias for fallible operations in this crate.
pub type Result<T> = std::result::Result<T, Error>;
