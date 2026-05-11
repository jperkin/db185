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

//! Iterate a real `pkgdb.byfile.db` and check the output against a
//! baseline produced by `pkg_admin dump`.  Both the database and the
//! expected dump are stored zstd-compressed under `tests/data/`.

use anyhow::{Context, Result};
use db185::Db;
use std::io::Write;
use tempfile::NamedTempFile;

const PKGDB: &[u8] = include_bytes!("data/pkgdb.byfile.db.zst");
const BASELINE: &[u8] = include_bytes!("data/pkgdb.dump.txt.zst");

#[test]
fn dump_matches_pkg_admin() -> Result<()> {
    let raw_db = zstd::decode_all(PKGDB).context("decompressing pkgdb sample")?;
    let baseline = zstd::decode_all(BASELINE).context("decompressing dump baseline")?;

    let mut tmp = NamedTempFile::new()?;
    tmp.write_all(&raw_db)?;
    tmp.flush()?;

    let db = Db::open(tmp.path())?;
    let mut actual = Vec::with_capacity(baseline.len());
    for entry in &db {
        let entry = entry?;
        actual.extend_from_slice(b"file: ");
        actual.extend_from_slice(trim_nul(entry.key()));
        actual.extend_from_slice(b" pkg: ");
        actual.extend_from_slice(trim_nul(entry.value()));
        actual.push(b'\n');
    }

    assert_eq!(actual.len(), baseline.len(), "dump length differs");
    assert!(
        actual == baseline,
        "dump output differs from pkg_admin baseline"
    );
    Ok(())
}

fn trim_nul(b: &[u8]) -> &[u8] {
    b.strip_suffix(b"\0").unwrap_or(b)
}
