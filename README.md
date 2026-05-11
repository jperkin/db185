# Pure Rust reader for BSD 4.4 dbopen(3) btree (db 1.85) files

[![Crates.io](https://img.shields.io/crates/v/db185.svg)](https://crates.io/crates/db185)
[![Documentation](https://docs.rs/db185/badge.svg)](https://docs.rs/db185)
[![License](https://img.shields.io/crates/l/db185.svg)](https://github.com/jperkin/db185)

`db185` reads the on-disk btree format used by the historical 4.4BSD
`dbopen(3)` interface, as preserved in NetBSD `libc` and the pkgsrc
`libnbcompat` sources.  It exists because
[pkgsrc-rs](https://github.com/jperkin/pkgsrc-rs) will need to read pkgsrc's
`pkgdb.byfile.db`, and the only pre-existing options for doing that are
NetBSD `libc` (NetBSD-only) or linking against the `libnbcompat` C sources
(everywhere else).

The crate targets only the subset of `dbopen` that `pkgdb.byfile.db`
actually uses: btree, no duplicates, default `memcmp` ordering, no
user-supplied compare or prefix callbacks.  Hash and recno access methods
are not supported and won't be.  Write support will follow.

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

The bundled `dump` example produces output byte-identical to
`pkg_admin dump` when run against a `pkgdb.byfile.db`, which is how
correctness is checked during development.  On a real 199,996-entry
pkgdb it runs in around 15 ms versus 35 ms for `pkg_admin`.
