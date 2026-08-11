# Bounded differential fuzzing

This independent `cargo-fuzz` workspace keeps Nightly, `libfuzzer-sys`, and
fuzz-only dependency resolution out of the Rust-1.77 root crate and normal CI.

Targets use only public scanner primitives and raw-byte decoders. Their corpus
is synthetic and intentionally small; generated corpora, artifacts, coverage,
and build outputs are ignored.

Run on a supported Nightly/libFuzzer host:

```sh
cargo +nightly fuzz run --sanitizer none scanner_differential -- -max_len=4352 -max_total_time=90 -timeout=5 -rss_limit_mb=1024
cargo +nightly fuzz run --sanitizer none fixed_extremes -- -max_len=32 -max_total_time=90 -timeout=5 -rss_limit_mb=1024
```

`scanner_differential` caps source data at 4 KiB, patterns at 32 bytes, and
requested partitions at 64. `fixed_extremes` uses an independent `u128` oracle
and includes fixed extreme-value classes in addition to raw 64-bit inputs.
The current CI smoke lane uses coverage instrumentation without a sanitizer.
The POSIX `open` ABI was corrected after this lane was introduced; ASan
compatibility should be re-checked on a supported Linux host before enabling it
in CI.
