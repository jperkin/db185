# Rust support for Berkeley DB 1.85

[![Crates.io](https://img.shields.io/crates/v/db185.svg)](https://crates.io/crates/db185)
[![Documentation](https://docs.rs/db185/badge.svg)](https://docs.rs/db185)
[![License](https://img.shields.io/crates/l/db185.svg)](https://github.com/jperkin/db185)

`db185` reads and writes Berkeley DB 1.85 btree files - the on-disk
format used by 4.4BSD's `dbopen(3)` and still shipped today in
NetBSD `libc` and pkgsrc's `libnbcompat`.  It exists primarily for
[pkgsrc-rs](https://github.com/jperkin/pkgsrc-rs) to read and update
pkgsrc's `pkgdb.byfile.db`.

What's supported:

- **Reader:** any Berkeley DB 1.85 btree file, in either byte order.
- **Writer:** builds a new file from scratch.  Each key/value pair
  must fit in a single page (~4 KiB).  That's enough for pkgsrc
  and similar workloads where keys are file paths and values are
  small identifiers; it is not a full Berkeley DB 1.85 writer.

Hash and recno access methods are out of scope.

```rust
use db185::Db;

let db = Db::open("/var/db/pkg/pkgdb.byfile.db")?;

if let Some(value) = db.get(b"/opt/pkg/bin/foo\0")? {
    println!("{}", String::from_utf8_lossy(value.as_ref()));
}

for entry in &db {
    let entry = entry?;
    println!(
        "{} -> {}",
        String::from_utf8_lossy(entry.key()),
        String::from_utf8_lossy(entry.value()),
    );
}
```

## Performance

Numbers from a desktop Apple Silicon machine with a ~190k-entry
`pkgdb.byfile.db`, measured with `hyperfine` via `tools/bench.sh`:

| workload                       | C (`pkg_install` / `libnbcompat`) | Rust (`db185`) | speedup |
|--------------------------------|-----------------------------------|----------------|---------|
| Full dump (read + iterate)     | ~34 ms                            | ~14 ms         | 2.47x   |
| Replay rebuild trace (writer)  | ~164 ms                           | ~78 ms         | 2.11x   |
| End-to-end `pkg_admin rebuild` | ~514 ms                           | ~558 ms        | 0.92x   |

The end-to-end rebuild row is slower in Rust because of overhead in
`pkgsrc-rs`'s filesystem walk and `+CONTENTS` parsing, not in db185 itself.
This will be fixed in due course.
