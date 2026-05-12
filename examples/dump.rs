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

//! Dump a Berkeley DB 1.85 btree file, or look up a single key.
//!
//! Usage:
//!
//! ```text
//! dump <file.db>          # iterate all key/value pairs
//! dump <file.db> <key>    # print the value for <key>, or nothing if missing
//! ```
//!
//! The no-key form produces output byte-identical to `pkg_admin dump`
//! when `<file.db>` is the pkgsrc `pkgdb.byfile.db`: same `file: <key>
//! pkg: <value>` line format and the same in-tree ordering, so the two
//! can be diffed directly with no post-processing.
//!
//! Keys in `pkgdb.byfile.db` are NUL-terminated.  A trailing NUL is
//! appended automatically to the supplied key, and a trailing NUL on
//! the value is trimmed for display.
//!
//! Exit codes: `0` on success, `1` when a requested key is absent, `2`
//! on usage error, and any other non-zero on I/O or format errors
//! (anyhow prints the error chain to stderr).

use anyhow::{Context, Result};
use db185::Db;
use std::env;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

fn main() -> Result<ExitCode> {
    let mut args = env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dump <file.db> [key]");
        return Ok(ExitCode::from(2));
    };
    let key = args.next();
    if args.next().is_some() {
        eprintln!("usage: dump <file.db> [key]");
        return Ok(ExitCode::from(2));
    }

    let db = Db::open(&path).with_context(|| format!("opening {}", path.to_string_lossy()))?;

    if let Some(k) = key {
        let mut bytes = k.to_string_lossy().into_owned().into_bytes();
        bytes.push(0);
        let value = db
            .get(&bytes)
            .with_context(|| format!("looking up {}", k.to_string_lossy()))?;
        value.map_or(Ok(ExitCode::from(1)), |v| {
            println!("{}", String::from_utf8_lossy(trim_nul(&v)));
            Ok(ExitCode::SUCCESS)
        })
    } else {
        let mut out = BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
        for entry in &db {
            let entry = entry.context("iterating database")?;
            write_pair(&mut out, entry.key(), entry.value()).context("writing output")?;
        }
        out.flush().context("flushing output")?;
        Ok(ExitCode::SUCCESS)
    }
}

fn write_pair<W: Write>(out: &mut W, key: &[u8], val: &[u8]) -> std::io::Result<()> {
    out.write_all(b"file: ")?;
    out.write_all(trim_nul(key))?;
    out.write_all(b" pkg: ")?;
    out.write_all(trim_nul(val))?;
    out.write_all(b"\n")
}

fn trim_nul(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\0").unwrap_or(bytes)
}
