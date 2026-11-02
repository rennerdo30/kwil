# Compressed text (KW-49)

GUI and TUI open gzip, zstd, bzip2 and xz by signature, regardless of the file
extension. The status bar identifies the container. Encoding detection runs on
the decompressed bytes; Save and Save As keep the container and text encoding.
Recompression preserves text, including malformed encoded bytes, rather than
the original compressed bitstream or container metadata.

**Save uncompressed as…** exports an atomic plain-text copy to a different path.
It keeps the current compressed document, its unsaved edits and recovery policy
intact. Subsequent Save still writes the compressed original. The command is in
the File menu and both command palettes. Open as Hex explicitly opens physical
container bytes when raw inspection is needed.

Decompression and encoding use the existing worker-owned private disk backings.
The decompressed body is force-mapped even for small files. Transfers use 64 KiB
chunks; snapshots keep backing owners alive until worker retirement. zstd's
decode window is capped at 64 MiB; xz's dictionary plus estimated decoder overhead
is capped at 64 MiB. Files requiring larger dictionaries fail with a clear error.
These limits bound codec memory, not logical document size. bzip2's block-size
limit is intrinsic to the format. XZ output uses preset 1 and a single block so
encoder index records stay constant in memory. Input XZ indexes are read
incrementally. Concatenated streams are supported. Truncation and checksum errors
abort opening and retire partial private files.

The atomic save path streams encoded text into the container, validates and
prepares its disk-backed checkpoint, then replaces the destination. Failed
preparation leaves the original intact. Journal codec version 4 records the
additional formats; existing raw/gzip version 1–3 journals remain readable.

Added dependencies: bzip2 0.6.1 (MIT OR Apache-2.0, Rust backend),
lzma-rust2 0.25.0 (Apache-2.0, single-threaded std/encoder/xz features), and
zstd 0.14.3 (BSD-3-Clause, default features disabled). These add six lockfile
packages; zstd includes a native C build on a cold cache. No asynchronous runtime
or codec worker pool is introduced. `about.toml` accepts the permissive
`bzip2-1.0.6` notice specifically for `libbz2-rs-sys`; every dependency remains
subject to the existing license check and notices generation.
