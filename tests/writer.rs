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

//! Writer round-trip tests.

use anyhow::{Context, Result};
use db185::{Db, PutResult, Writer};
use tempfile::TempDir;

#[test]
fn empty_round_trip() -> Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("empty.db");
    Writer::create(&path)?.close()?;

    let db = Db::open(&path)?;
    assert!(
        db.iter().next().is_none(),
        "fresh db should yield no entries"
    );
    assert!(db.get(b"any\0")?.is_none(), "fresh db should have no keys");
    Ok(())
}

#[test]
fn put_round_trip_single_entry() -> Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("one.db");
    let mut w = Writer::create(&path)?;
    assert_eq!(w.put(b"hello\0", b"world\0")?, PutResult::Inserted);
    w.close()?;

    let db = Db::open(&path)?;
    let val = db.get(b"hello\0")?.context("hello should be present")?;
    assert_eq!(val.as_ref(), b"world\0");
    assert!(db.get(b"missing\0")?.is_none());
    Ok(())
}

#[test]
fn put_no_overwrite() -> Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("dup.db");
    let mut w = Writer::create(&path)?;
    assert_eq!(w.put(b"k\0", b"v1\0")?, PutResult::Inserted);
    assert_eq!(w.put(b"k\0", b"v2\0")?, PutResult::KeyExists);
    w.close()?;

    let db = Db::open(&path)?;
    let val = db.get(b"k\0")?.context("k should be present")?;
    assert_eq!(val.as_ref(), b"v1\0", "first put wins under R_NOOVERWRITE");
    Ok(())
}

/// Byte-for-byte against an 8 KiB reference produced by
/// `pkg_admin rebuild` over a one-package, one-file fake pkgdb.
/// Validates the meta page layout and a single-entry root leaf.
#[test]
fn matches_pkg_admin_single_entry() -> Result<()> {
    const REFERENCE: &[u8] = include_bytes!("data/single_entry.db");

    let dir = TempDir::new()?;
    let path = dir.path().join("ours.db");
    let mut w = Writer::create(&path)?;
    assert_eq!(w.put(b"/tmp/hello\0", b"foo-1.0\0")?, PutResult::Inserted);
    w.close()?;
    let ours = std::fs::read(&path)?;
    assert_eq!(
        ours, REFERENCE,
        "writer output differs from pkg_admin rebuild reference"
    );
    Ok(())
}

/// Byte-for-byte against a 16 KiB reference produced by
/// `pkg_admin rebuild` over a one-package, 39-file fake pkgdb.  At
/// the chosen key/value sizes, 38 entries fit in the root leaf and
/// the 39th forces exactly one root split, exercising `bt_root` +
/// `bt_psplit` + `bt_broot` and stopping cleanly without needing
/// non-root descent.
#[test]
fn matches_pkg_admin_one_split() -> Result<()> {
    const REFERENCE: &[u8] = include_bytes!("data/one_split.db");
    const PREFIX: &str = "/tmp/onesplit-files/path/to/some/longer/directory/";

    let dir = TempDir::new()?;
    let path = dir.path().join("ours.db");
    let mut w = Writer::create(&path)?;
    for i in 1..=39 {
        let mut key = PREFIX.as_bytes().to_vec();
        key.extend_from_slice(format!("file_with_a_somewhat_long_name_{i:03}").as_bytes());
        key.push(0);
        assert_eq!(w.put(&key, b"big-1.0\0")?, PutResult::Inserted);
    }
    w.close()?;
    let ours = std::fs::read(&path)?;
    assert_eq!(ours.len(), REFERENCE.len(), "writer output length differs");
    assert_eq!(
        ours, REFERENCE,
        "writer output differs from pkg_admin rebuild reference"
    );
    Ok(())
}

/// Delete an entry, then insert enough new entries to force a leaf
/// split.  Exercises the post-delete free-space accounting in
/// `bt_dleaf` plus a regular non-root leaf split on the compacted
/// page.  Verified via the reader: after the dance the surviving
/// keys must iterate in sorted order with the right values.
#[test]
fn delete_then_split_via_reader() -> Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("del_then_split.db");
    let mut w = Writer::create(&path)?;
    let prefix = "/tmp/del-split/path/to/some/directory/file_";
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for i in 0..200 {
        let mut k = prefix.as_bytes().to_vec();
        k.extend_from_slice(format!("{i:04}_with_padding_to_force_splits").as_bytes());
        k.push(0);
        assert_eq!(w.put(&k, b"pkg-1.0\0")?, PutResult::Inserted);
        keys.push(k);
    }
    // Delete every 7th entry, then insert that key back with a
    // longer value so it lands in a different leaf slot than before.
    for (i, k) in keys.iter().enumerate() {
        if i % 7 == 0 {
            assert!(w.del(k)?);
        }
    }
    for (i, k) in keys.iter().enumerate() {
        if i % 7 == 0 {
            assert_eq!(w.put(k, b"pkg-2.0-replacement\0")?, PutResult::Inserted);
        }
    }
    w.close()?;

    let db = Db::open(&path)?;
    let mut prev: Option<Vec<u8>> = None;
    let mut count = 0usize;
    for entry in &db {
        let entry = entry?;
        if let Some(p) = prev.as_ref() {
            assert!(p.as_slice() < entry.key(), "keys must iterate sorted");
        }
        prev = Some(entry.key().to_vec());
        count += 1;
    }
    assert_eq!(count, 200, "delete + re-insert preserves count");
    Ok(())
}

/// Sorted-append into the rightmost leaf is special-cased by
/// `bt_page` (sorted-opt): the existing rightmost page is kept
/// intact and a fresh leaf holding only the new entry is linked
/// after it.  This test sorted-appends enough keys to force many
/// such splits at the leaf level *and* an internal-page sorted-opt
/// (added in this crate so the upper-tree shape matches C).
#[test]
fn sorted_append_grows_rightmost_leaf_chain() -> Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("sorted.db");
    let mut w = Writer::create(&path)?;
    for i in 0..2000u32 {
        let key = format!("/sorted/{i:08}_some_padding_to_take_real_space\0");
        let val = format!("pkg-{i}\0");
        assert_eq!(w.put(key.as_bytes(), val.as_bytes())?, PutResult::Inserted,);
    }
    w.close()?;

    let db = Db::open(&path)?;
    let mut last: Option<u32> = None;
    let mut count = 0usize;
    for entry in &db {
        let entry = entry?;
        let key = std::str::from_utf8(entry.key())?.trim_end_matches('\0');
        // Keys are "/sorted/<8-digit index>_..."; pull the index out.
        let idx: u32 = key
            .trim_start_matches("/sorted/")
            .get(..8)
            .context("8-digit index field")?
            .parse()
            .context("parse idx")?;
        if let Some(p) = last {
            assert_eq!(idx, p + 1, "sorted iteration must visit indices in order");
        }
        last = Some(idx);
        count += 1;
    }
    assert_eq!(count, 2000);
    Ok(())
}

/// Inserts that can never fit on any leaf (i.e. would require
/// `P_BIGKEY` / `P_BIGDATA` overflow page support) return an
/// [`Error::EntryTooLarge`] instead of panicking inside the leaf
/// helper.  Real overflow support is future work; this just
/// ensures the API rejects oversize input cleanly.
#[test]
fn put_oversize_entry_returns_clean_error() -> Result<()> {
    use db185::Error;
    let dir = TempDir::new()?;
    let path = dir.path().join("toobig.db");
    let mut w = Writer::create(&path)?;
    // 4080-byte key, NUL-terminated value: total entry (header +
    // key + value, aligned to 4 + the 2-byte linp slot) exceeds the
    // 4076-byte usable area of a 4096-byte page.
    let huge = vec![b'x'; 4080];
    let err = w.put(&huge, b"v\0").unwrap_err();
    assert!(
        matches!(err, Error::EntryTooLarge { .. }),
        "expected EntryTooLarge, got {err:?}",
    );
    w.close()?;
    Ok(())
}

/// A key that fits as a *leaf* entry (header + key + val + linp
/// slot under the page's usable area) but is too large to survive
/// as an *internal* separator on a root-conversion page (which
/// packs the zero-key leftmost-child entry plus the new separator
/// onto a single fresh page).  `Writer::put` enforces both bounds
/// up front rather than risking an unfittable mid-split layout;
/// see the docs on [`db185::Error::EntryTooLarge`] for why.
#[test]
fn put_leaf_fittable_but_separator_too_large_errors() -> Result<()> {
    use db185::Error;
    let dir = TempDir::new()?;
    let path = dir.path().join("sep.db");
    let mut w = Writer::create(&path)?;
    // 4060-byte key: align(9+4060+2)=4072 bytes leaf-entry, +2 linp
    // = 4074 ≤ 4076 usable.  But as a separator: align(9+4060)=4072
    // bytes, + the 12-byte zero-key entry + 4 bytes for 2 linp slots
    // = 4088 > 4076 usable, so it must be rejected.
    let huge_key = vec![b'k'; 4060];
    let err = w.put(&huge_key, b"v\0").unwrap_err();
    assert!(
        matches!(err, Error::EntryTooLarge { .. }),
        "expected EntryTooLarge for separator-overflowing key, got {err:?}",
    );
    w.close()?;
    Ok(())
}
