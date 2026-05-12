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

//! Replay the captured `pkg_admin rebuild` operation trace through
//! the writer and byte-compare the output against the reference
//! `pkgdb.byfile.db` produced by the same `pkg_admin` run.
//!
//! This is the end-to-end correctness test that drives write
//! development: as features land, the first diff offset (or first
//! panic) moves further into the trace.

use anyhow::{Context, Result, bail};
use db185::{Db, Writer};
use std::fmt::Write as _;
use tempfile::TempDir;

const TRACE: &[u8] = include_bytes!("data/pkgdb.byfile.trace.zst");
const REFERENCE: &[u8] = include_bytes!("data/pkgdb.byfile.db.zst");

const OP_PUT: u8 = 0;
const OP_DEL: u8 = 1;

#[test]
#[allow(clippy::too_many_lines)] // single-end-to-end test reads better whole
fn replay_trace_matches_pkg_admin_rebuild() -> Result<()> {
    let trace = zstd::decode_all(TRACE).context("decompressing trace")?;
    let reference = zstd::decode_all(REFERENCE).context("decompressing reference")?;

    let dir = TempDir::new()?;
    let path = dir.path().join("replay.db");
    let mut writer = Writer::create(&path)?;

    let mut cursor = 0usize;
    let mut op_index = 0usize;
    while cursor < trace.len() {
        let op = trace[cursor];
        cursor += 1;
        let key = read_slice(&trace, &mut cursor, op_index)?;
        match op {
            OP_PUT => {
                let val = read_slice(&trace, &mut cursor, op_index)?;
                writer.put(key, val).with_context(|| {
                    format!(
                        "trace op {op_index}: put key={:?}",
                        String::from_utf8_lossy(key)
                    )
                })?;
            }
            OP_DEL => {
                writer.del(key).with_context(|| {
                    format!(
                        "trace op {op_index}: del key={:?}",
                        String::from_utf8_lossy(key)
                    )
                })?;
            }
            other => bail!("trace op {op_index}: unknown op byte {other:#x}"),
        }
        op_index += 1;
    }
    writer.close()?;

    let ours = std::fs::read(&path)?;

    // Strict byte equality is the strongest possible check, but it
    // also depends on bytes that libnbcompat *leaves unspecified*
    // (entry-alignment padding inside aligned entries, and the
    // free-space slack between `lower` and `upper`).  Two
    // back-to-back runs of `c-replay` on the same trace produce
    // millions of byte differences in those regions purely from
    // stack/heap state, so requiring strict byte equality against
    // a captured reference would require us to reproduce malloc
    // state that's process-specific.  The defensible correctness
    // property is *active-byte* equality: every page header, every
    // `linp[]` value, every entry's written fields (header + key
    // + value for leaves; header + key for internals) match.  That
    // is what we check below.
    if ours == reference {
        return Ok(());
    }

    let ref_path = dir.path().join("reference.db");
    std::fs::write(&ref_path, &reference)?;

    let stash_ours = std::path::Path::new("/tmp/db185-replay-ours.db");
    let stash_ref = std::path::Path::new("/tmp/db185-replay-ref.db");
    let _ = std::fs::remove_file(stash_ours);
    let _ = std::fs::remove_file(stash_ref);
    std::fs::write(stash_ours, &ours)?;
    std::fs::write(stash_ref, &reference)?;

    // A page-count mismatch on its own is a regression even before
    // we look at active bytes: `canon_active_diff` only compares
    // the common prefix and would otherwise hide extra trailing
    // pages or a missing page.
    if ours.len() != reference.len() {
        bail!(
            "page-count regression: ours.len()={} ({} pages), reference.len()={} ({} pages)",
            ours.len(),
            ours.len() / PSIZE_CANON,
            reference.len(),
            reference.len() / PSIZE_CANON,
        );
    }

    let (active, _total_pages, first_active) = canon_active_diff(&ours, &reference);
    if active == 0 {
        // Trees are byte-identical in every byte the writer is
        // responsible for - the only differences are unspecified
        // padding/free-space.  Treat that as success.
        return Ok(());
    }

    let content_diff = content_diff_summary(&path, &ref_path)?;

    let mut msg = String::new();
    let _ = writeln!(msg, "{content_diff}");
    let _ = writeln!(
        msg,
        "writer output differs from reference in *active* bytes. \
         ours.len={} ({} pages), ref.len={} ({} pages), {active} active-byte diffs",
        ours.len(),
        ours.len() / 4096,
        reference.len(),
        reference.len() / 4096,
    );
    if let Some((off, pgno, byte)) = first_active {
        let _ = writeln!(
            msg,
            "first active diff at offset {off} (page {pgno}, byte {byte}): ours = {:#04x}, ref = {:#04x}",
            ours[off], reference[off],
        );
        diff_internal_page(&mut msg, "ours", &ours, pgno);
        diff_internal_page(&mut msg, "ref ", &reference, pgno);
    }
    bail!("{msg}");
}

const PSIZE_CANON: usize = 4096;
const PAGE_HEADER_SIZE_CANON: usize = 20;
const BLEAF_HEADER_SIZE_CANON: usize = 9;
const BINT_HEADER_SIZE_CANON: usize = 9;
const P_BINTERNAL_CANON: u32 = 0x01;
const P_BLEAF_CANON: u32 = 0x02;

/// Count active-byte differences between `a` and `b`, ignoring
/// padding inside aligned entries and free-space slack between
/// `lower` and `upper`.  See the doc comment on this test for why
/// strict byte equality is not the right property to check.
fn canon_active_diff(a: &[u8], b: &[u8]) -> (u64, usize, Option<(usize, usize, usize)>) {
    let n_pages = a.len().min(b.len()) / PSIZE_CANON;
    let mut active = 0u64;
    let mut first: Option<(usize, usize, usize)> = None;
    for pgno in 0..n_pages {
        let base = pgno * PSIZE_CANON;
        let ap: &[u8; PSIZE_CANON] = a[base..base + PSIZE_CANON].try_into().expect("psize");
        let bp: &[u8; PSIZE_CANON] = b[base..base + PSIZE_CANON].try_into().expect("psize");
        if pgno == 0 {
            for i in 0..PSIZE_CANON {
                if ap[i] != bp[i] {
                    active += 1;
                    if first.is_none() {
                        first = Some((base + i, pgno, i));
                    }
                }
            }
            continue;
        }
        let ma = canon_active_mask(ap);
        let mb = canon_active_mask(bp);
        for i in 0..PSIZE_CANON {
            if ap[i] == bp[i] {
                continue;
            }
            if ma[i] || mb[i] {
                active += 1;
                if first.is_none() {
                    first = Some((base + i, pgno, i));
                }
            }
        }
    }
    (active, n_pages, first)
}

// NOTE: the current writer never emits overflow (`P_OVERFLOW`)
// pages - oversized entries are rejected up front - so the
// active-byte mask below intentionally only handles leaf and
// internal page types.  When write-side overflow support lands,
// this function must grow a `P_OVERFLOW` arm that marks the
// 20-byte header plus the first `size` payload bytes (where
// `size` comes from the chain's owning `BLEAF` / `BINTERNAL`
// `{pgno, size}` ref) as active, so overflow regressions don't
// hide in unwritten payload slack.
fn canon_active_mask(page: &[u8; PSIZE_CANON]) -> [bool; PSIZE_CANON] {
    let mut mask = [false; PSIZE_CANON];
    mask[..PAGE_HEADER_SIZE_CANON].fill(true);
    let flags = u32::from_ne_bytes([page[12], page[13], page[14], page[15]]);
    let lower = u16::from_ne_bytes([page[16], page[17]]) as usize;
    if !(PAGE_HEADER_SIZE_CANON..=PSIZE_CANON).contains(&lower) {
        mask.fill(true);
        return mask;
    }
    mask[PAGE_HEADER_SIZE_CANON..lower].fill(true);
    let is_leaf = flags & P_BLEAF_CANON != 0;
    let is_internal = flags & P_BINTERNAL_CANON != 0;
    if !(is_leaf || is_internal) {
        return mask;
    }
    let n = (lower - PAGE_HEADER_SIZE_CANON) / 2;
    for i in 0..n {
        let off = u16::from_ne_bytes([
            page[PAGE_HEADER_SIZE_CANON + i * 2],
            page[PAGE_HEADER_SIZE_CANON + i * 2 + 1],
        ]) as usize;
        let hdr = if is_leaf {
            BLEAF_HEADER_SIZE_CANON
        } else {
            BINT_HEADER_SIZE_CANON
        };
        if off + hdr > PSIZE_CANON {
            continue;
        }
        let ksize =
            u32::from_ne_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]) as usize;
        let entry_active = if is_leaf {
            let dsize =
                u32::from_ne_bytes([page[off + 4], page[off + 5], page[off + 6], page[off + 7]])
                    as usize;
            BLEAF_HEADER_SIZE_CANON + ksize + dsize
        } else {
            BINT_HEADER_SIZE_CANON + ksize
        };
        let end = (off + entry_active).min(PSIZE_CANON);
        mask[off..end].fill(true);
    }
    mask
}

/// Dump an internal page in decoded form: header fields plus each
/// linp entry's `(ksize, child_pgno, key_prefix)`.
fn diff_internal_page(msg: &mut String, label: &str, buf: &[u8], pgno: usize) {
    let base = pgno * 4096;
    let psize = 4096;
    if base + psize > buf.len() {
        return;
    }
    let page = &buf[base..base + psize];
    let read_u16 = |off: usize| u16::from_ne_bytes([page[off], page[off + 1]]);
    let read_u32 =
        |off: usize| u32::from_ne_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
    let self_pgno = read_u32(0);
    let prev_pg = read_u32(4);
    let next_pg = read_u32(8);
    let flags = read_u32(12);
    let lower = read_u16(16) as usize;
    let upper = read_u16(18) as usize;
    let nentries = lower.saturating_sub(20) / 2;
    let _ = writeln!(
        msg,
        "--- {label} page {pgno}: self={self_pgno} prev={prev_pg} next={next_pg} \
         flags={flags:#x} lower={lower} upper={upper} nentries={nentries} ---",
    );
    let n_show = nentries.min(24);
    for i in 0..n_show {
        let linp = read_u16(20 + i * 2) as usize;
        if linp + 9 > psize {
            let _ = writeln!(msg, "  linp[{i}] = {linp} (out of range)");
            continue;
        }
        let ksize = u32::from_ne_bytes([page[linp], page[linp + 1], page[linp + 2], page[linp + 3]])
            as usize;
        let child = u32::from_ne_bytes([
            page[linp + 4],
            page[linp + 5],
            page[linp + 6],
            page[linp + 7],
        ]);
        let eflags = page[linp + 8];
        let key_end = (linp + 9 + ksize).min(psize);
        let key = &page[linp + 9..key_end];
        let key_show = key
            .iter()
            .take(40)
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect::<String>();
        let _ = writeln!(
            msg,
            "  linp[{i}] off={linp} ksize={ksize} child={child} eflags={eflags:#x} key={key_show:?}",
        );
    }
    if nentries > n_show {
        let _ = writeln!(msg, "  ... ({} more)", nentries - n_show);
    }
}

/// Iterate both DB files in sorted order, comparing the resulting
/// `(key, value)` sets.  Returns a one-line summary like "contents
/// equal", or a description of the first mismatch / count drift.
fn content_diff_summary(ours_path: &std::path::Path, ref_path: &std::path::Path) -> Result<String> {
    let ours = Db::open(ours_path)?;
    let theirs = Db::open(ref_path)?;
    let mut ours_iter = (&ours).into_iter();
    let mut theirs_iter = (&theirs).into_iter();
    let mut count = 0usize;
    loop {
        let a = ours_iter.next();
        let b = theirs_iter.next();
        match (a, b) {
            (None, None) => return Ok(format!("contents equal: {count} entries match")),
            (Some(_), None) => {
                let mut msg = format!("ours has more entries: ref ran out at {count}");
                while ours_iter.next().is_some() {
                    count += 1;
                }
                let _ = write!(&mut msg, " (ours total {count})");
                return Ok(msg);
            }
            (None, Some(_)) => {
                let mut msg = format!("ref has more entries: ours ran out at {count}");
                while theirs_iter.next().is_some() {
                    count += 1;
                }
                let _ = write!(&mut msg, " (ref total {count})");
                return Ok(msg);
            }
            (Some(a), Some(b)) => {
                let ea = a?;
                let eb = b?;
                if ea.key() != eb.key() || ea.value() != eb.value() {
                    return Ok(format!(
                        "content diverges at entry {count}: ours=({:?}, {:?}) ref=({:?}, {:?})",
                        String::from_utf8_lossy(ea.key()),
                        String::from_utf8_lossy(ea.value()),
                        String::from_utf8_lossy(eb.key()),
                        String::from_utf8_lossy(eb.value()),
                    ));
                }
                count += 1;
            }
        }
    }
}

fn read_slice<'a>(buf: &'a [u8], cursor: &mut usize, op: usize) -> Result<&'a [u8]> {
    let off = *cursor;
    if off + 4 > buf.len() {
        bail!("trace op {op}: truncated length");
    }
    let len = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
    *cursor = off + 4;
    if *cursor + len > buf.len() {
        bail!("trace op {op}: truncated payload (need {len} bytes)");
    }
    let slice = &buf[*cursor..*cursor + len];
    *cursor += len;
    Ok(slice)
}
