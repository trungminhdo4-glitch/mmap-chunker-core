//! Performance experiment: scalar vs safe SWAR delimiter search.
//!
//! Phases 1–11 of the SWAR performance evaluation.
//! Run:  cargo test --test performance --release -- --nocapture --ignored
//!
//! This file contains:
//!   - Phase 2: find_byte_scalar (reference)
//!   - Phase 4: find_byte_swar  (safe SWAR prototype)
//!   - Phase 3+5: correctness oracle (exhaustive test matrix)
//!   - Phase 1+8+9: search amplification + realistic workload matrix
//!   - Phase 6+7: microbenchmark byte search (scalar vs SWAR)
//!   - Phase 10+11: end-to-end scanner + mmap benchmark
//!   - Phase 12: optional memchr calibration (behind feature flag)

use mmap_chunker_core::scanner::find_chunk_boundaries;
use std::hint::black_box;
use std::time::Instant;

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 2: FIND_BYTE_SCALAR — reference primitive
// ═══════════════════════════════════════════════════════════════════════════════

/// Find first occurrence of `delimiter` in `haystack`.
/// Semantically identical to `haystack.iter().position(|&b| b == delimiter)`.
#[inline(always)]
pub fn find_byte_scalar(haystack: &[u8], delimiter: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == delimiter)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 4: FIND_BYTE_SWAR — safe 64-bit word-at-a-time search
// ═══════════════════════════════════════════════════════════════════════════════

/// Safe SWAR (SIMD Within A Register) byte search.
///
/// Strategy:
///   1. Scalar prefix to reach 8-byte alignment
///   2. SWAR main loop: load 8 bytes as u64, XOR with broadcast delimiter,
///      detect zero bytes via classic `haszero` bit-hack
///   3. Scalar tail for remaining <8 bytes
///
/// No unsafe. No new dependencies. MSRV 1.77 compatible.
#[inline(never)]
pub fn find_byte_swar(haystack: &[u8], delimiter: u8) -> Option<usize> {
    let len = haystack.len();
    if len == 0 {
        return None;
    }

    let pattern = (delimiter as u64).wrapping_mul(0x0101010101010101u64);
    let lo = 0x0101010101010101u64;
    let hi = 0x8080808080808080u64;

    let mut i = 0usize;

    // Phase 1: scalar prefix to reach 8-byte alignment
    let ptr = haystack.as_ptr() as usize;
    let align = ptr % 8;
    if align != 0 {
        let prefix_end = (8 - align).min(len);
        while i < prefix_end {
            if haystack[i] == delimiter {
                return Some(i);
            }
            i += 1;
        }
    }

    // Phase 2: SWAR main loop (aligned 8-byte reads)
    while i + 8 <= len {
        let chunk: [u8; 8] = haystack[i..i + 8].try_into().unwrap();
        let word = u64::from_ne_bytes(chunk);
        let xored = word ^ pattern;

        let has_zero = xored.wrapping_sub(lo) & !xored & hi;
        if has_zero != 0 {
            return Some(i + (has_zero.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }

    // Phase 3: scalar tail (< 8 bytes)
    while i < len {
        if haystack[i] == delimiter {
            return Some(i);
        }
        i += 1;
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 3+5: CORRECTNESS ORACLE — exhaustive test matrix
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod correctness {
    use super::*;

    const DELIMITERS: &[u8] = &[0x00, 0x01, b'\n', b',', b'|', 0x7f, 0x80, 0xfe, 0xff];
    const LENGTHS: &[usize] = &[
        0, 1, 2, 3, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 200, 256,
    ];

    /// Verify SWAR == scalar for named test case.
    fn assert_swar_eq_scalar(data: &[u8], delim: u8, label: &str) {
        let s = find_byte_scalar(data, delim);
        let w = find_byte_swar(data, delim);
        assert_eq!(
            s,
            w,
            "SWAR != scalar: {label}  data[0]={:02x} len={} delim={:02x}",
            data.first().copied().unwrap_or(0),
            data.len(),
            delim
        );
    }

    /// Generate deterministic "random" bytes from a seed.
    fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 32) as u8
            })
            .collect()
    }

    #[test]
    fn correctness_empty() {
        for &d in DELIMITERS {
            assert_swar_eq_scalar(&[], d, "empty");
        }
    }

    #[test]
    fn correctness_all_lengths_no_match() {
        for &len in LENGTHS {
            let delim = b'\n';
            let data = vec![b'x'; len];
            assert_swar_eq_scalar(&data, delim, &format!("len={len} nomatch"));
        }
    }

    #[test]
    fn correctness_all_lengths_all_match() {
        for &len in LENGTHS {
            if len == 0 {
                continue;
            }
            let delim = b'\n';
            let data = vec![delim; len];
            assert_swar_eq_scalar(&data, delim, &format!("len={len} allmatch"));
        }
    }

    #[test]
    fn correctness_match_at_every_position() {
        for &len in &[1, 8, 16, 32, 64, 128, 200] {
            for pos in 0..len {
                let delim = b'\n';
                let mut data = vec![b'x'; len];
                data[pos] = delim;
                assert_swar_eq_scalar(&data, delim, &format!("len={len} pos={pos}"));
            }
        }
    }

    #[test]
    fn correctness_all_delimiters() {
        let len = 256usize;
        for &delim in DELIMITERS {
            // No match
            let other: u8 = if delim == 0x00 { 0x01 } else { 0x00 };
            let data = vec![other; len];
            assert_swar_eq_scalar(&data, delim, &format!("delim={delim:02x} nomatch"));

            // Match at middle
            let mut data = vec![other; len];
            data[len / 2] = delim;
            assert_swar_eq_scalar(&data, delim, &format!("delim={delim:02x} middle"));
        }
    }

    #[test]
    fn correctness_unaligned_starts() {
        let base = deterministic_bytes(42, 256);
        for start in 0..16 {
            for &delim in &[b'\n', b',', 0x00, 0xff] {
                let slice = &base[start..];
                assert_swar_eq_scalar(
                    slice,
                    delim,
                    &format!("unaligned start={start} delim={delim:02x}"),
                );
            }
        }
    }

    #[test]
    fn correctness_match_at_boundaries() {
        let delim = b'\n';
        for &len in &[8, 9, 15, 16, 17, 24, 25, 31, 32, 33] {
            let mut data = vec![b'x'; len];
            // Match at word boundary positions
            for pos in [0, 7, 8, 15, 16, 23, 24, 31, 32].iter().copied() {
                if pos < len {
                    data[pos] = delim;
                    assert_swar_eq_scalar(
                        &data,
                        delim,
                        &format!("len={len} word-boundary pos={pos}"),
                    );
                    data[pos] = b'x'; // restore
                }
            }
            // Match at last byte
            data[len - 1] = delim;
            assert_swar_eq_scalar(&data, delim, &format!("len={len} last-byte"));
        }
    }

    #[test]
    fn correctness_deterministic_random() {
        for seed in 0..20u64 {
            let data = deterministic_bytes(seed, 1024);
            for delim in [b'\n', b',', b'|', 0x00, 0xff] {
                assert_swar_eq_scalar(
                    &data,
                    delim,
                    &format!("random seed={seed} delim={delim:02x}"),
                );
            }
        }
    }

    #[test]
    fn correctness_many_matches() {
        let delim = b'\n';
        let mut data = Vec::with_capacity(10000);
        for i in 0..1000 {
            data.extend_from_slice(b"line_content_");
            data.extend_from_slice(&(i as u32).to_le_bytes());
            data.push(delim);
        }
        for &delim_test in &[b'\n', b'l', b'_', 0x00, 0xff] {
            assert_swar_eq_scalar(
                &data,
                delim_test,
                &format!("many_matches delim={delim_test:02x}"),
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 6+7: MICROBENCHMARK — byte search primitive only
// ═══════════════════════════════════════════════════════════════════════════════

/// Run one method (scalar or SWAR) on a prepared buffer, return elapsed ns.
fn bench_find_byte<F>(haystack: &[u8], delim: u8, f: F, iters: u64) -> f64
where
    F: Fn(&[u8], u8) -> Option<usize>,
{
    let start = Instant::now();
    for _ in 0..iters {
        let result = f(black_box(haystack), delim);
        black_box(result);
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// Auto-calibrated benchmark: keep doubling iterations until RSD < 2% or max reached.
fn bench_find_byte_auto<F>(haystack: &[u8], delim: u8, f: F, name: &str) -> f64
where
    F: Fn(&[u8], u8) -> Option<usize>,
{
    let warmup_iters = 10;
    for _ in 0..warmup_iters {
        black_box(f(black_box(haystack), delim));
    }

    let mut iters: u64 = 100;
    let max_iters: u64 = 100_000;
    let mut samples = Vec::with_capacity(10);

    while iters <= max_iters {
        let total_ns = bench_find_byte(haystack, delim, &f, iters);
        if total_ns * iters as f64 > 500_000_000.0 {
            // ~500ms per sample — good enough precision
            samples.push(total_ns);
            if samples.len() >= 5 {
                break;
            }
        }
        iters = (iters * 2).min(max_iters);
        if iters == max_iters && samples.is_empty() {
            samples.push(total_ns);
            break;
        }
    }

    if samples.is_empty() {
        return 0.0;
    }

    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_ns = sorted[sorted.len() / 2];

    let result = black_box(f(black_box(haystack), delim));
    let _ = black_box(result);

    println!("  {name:<20} {median_ns:>10.1} ns/call");
    median_ns
}

#[test]
#[ignore = "performance experiment — run with --ignored --nocapture"]
fn bench_byte_search_primitive() {
    println!();
    println!("=== Microbenchmark: byte search primitive ===");

    let delim = b'\n';

    // Use generated data
    fn gen_data(len: usize, pattern: &str) -> Vec<u8> {
        match pattern {
            "no_match" => vec![b'x'; len],
            "match_mid" => {
                let mut d = vec![b'x'; len];
                d[len / 2] = b'\n';
                d
            }
            "match_end" => {
                let mut d = vec![b'x'; len];
                d[len - 1] = b'\n';
                d
            }
            "match_first" => {
                let mut d = vec![b'x'; len];
                d[0] = b'\n';
                d
            }
            _ => vec![b'x'; len],
        }
    }

    let sizes: &[(usize, &str)] = &[
        (32, "Tiny"),
        (64, "1 cache line"),
        (128, "2 cache lines"),
        (256, "4 cache lines"),
        (4096, "4 KiB page"),
        (65536, "64 KiB"),
        (1048576, "1 MiB"),
    ];

    println!("  Case                       Scalar (ns)   SWAR (ns)   Ratio");

    for &(size, label) in sizes {
        for pattern in ["no_match", "match_mid"] {
            let data = gen_data(size, pattern);
            let case = format!("{label} {size}B {pattern}");
            let s = bench_find_byte_auto(&data, delim, find_byte_scalar, &format!("scalar {case}"));
            let w = bench_find_byte_auto(&data, delim, find_byte_swar, &format!("SWAR   {case}"));
            let ratio = if s > 0.0 { w / s } else { f64::NAN };
            println!(
                "  {case:<25} {s:>14.1} {w:>14.1} {ratio:>9.2}x",
                case = case,
                s = s as f64,
                w = w as f64,
                ratio = ratio
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 1+8+9: SEARCH AMPLIFICATION + REALISTIC WORKLOAD MATRIX
// ═══════════════════════════════════════════════════════════════════════════════

/// Generate synthetic datasets with controlled record sizes.
struct DataGenerator {
    record_size: usize, // mean record size including delimiter
    delimiter: u8,
}

impl DataGenerator {
    fn new(record_size: usize, delimiter: u8) -> Self {
        Self {
            record_size,
            delimiter,
        }
    }

    fn generate(&self, total_bytes: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(total_bytes);
        let payload_size = self.record_size.saturating_sub(1);
        let mut pos = 0;
        while pos < total_bytes {
            let remain = (total_bytes - pos).min(payload_size);
            if remain == 0 {
                break;
            }
            // Fill with deterministic pattern based on position
            let fill_byte = (pos as u8).wrapping_add(0x41);
            data.resize(data.len() + remain, fill_byte);
            pos += remain;
            if pos < total_bytes {
                data.push(self.delimiter);
                pos += 1;
            }
        }
        data
    }
}

/// Instrumented version of find_chunk_boundaries that counts bytes examined
/// by the delimiter search.
fn find_chunk_boundaries_instrumented(
    data: &[u8],
    chunk_size: usize,
    delimiter: u8,
) -> (Vec<(usize, usize)>, usize) {
    if data.is_empty() {
        return (Vec::new(), 0);
    }

    let len = data.len();
    let step = chunk_size.max(1);
    let estimate = (len / step) + 2;
    let mut chunks = Vec::with_capacity(estimate);
    let mut search_bytes = 0usize;
    let mut start = 0usize;

    while start < len {
        let mut end = start + step;
        if end >= len {
            end = len;
        } else {
            let remainder = &data[end..];
            search_bytes += remainder.len();
            if let Some(rel_pos) = remainder.iter().position(|&b| b == delimiter) {
                end = end + rel_pos + 1;
                if end > len {
                    end = len;
                }
            } else {
                end = len;
            }
        }
        chunks.push((start, end));
        start = end;
    }

    (chunks, search_bytes)
}

#[test]
#[ignore = "performance experiment — run with --ignored --nocapture"]
fn bench_search_amplification() {
    println!();
    println!("=== Search Amplification Analysis ===");
    println!(
        "  Dataset                  Input       Chunk KB  Chunks   Search B        Ratio    Avg OS"
    );

    let workloads: &[(&str, usize)] = &[
        ("JSONL ~100B", 100),
        ("Log ~300B", 300),
        ("CSV ~2KiB", 2048),
        ("Sparse ~64KiB", 65536),
    ];

    let file_sizes: &[usize] = &[1_048_576, 16_777_216, 67_108_864]; // 1, 16, 64 MB
    let chunk_sizes_kb: &[usize] = &[64, 256, 1024];

    for &(label, record_size) in workloads {
        let gen = DataGenerator::new(record_size, b'\n');
        for &file_size in file_sizes {
            let data = gen.generate(file_size);
            for &chunk_kb in chunk_sizes_kb {
                let chunk_size = chunk_kb * 1024;
                let (chunks, search_bytes) =
                    find_chunk_boundaries_instrumented(&data, chunk_size, b'\n');

                let ratio = if file_size > 0 {
                    search_bytes as f64 / file_size as f64
                } else {
                    0.0
                };

                let avg_overshoot = if !chunks.is_empty() {
                    let total_os: usize = chunks
                        .iter()
                        .map(|(s, e)| (e - s).saturating_sub(chunk_size.min(e - s)))
                        .sum();
                    total_os as f64 / chunks.len() as f64
                } else {
                    0.0
                };

                println!(
                    "  {label:<20} {fsize:>10} MB {ckb:>8} KB {clen:<8} {sb:>10} B {ratio:>9.4} {avg_os:>10.1}",
                    label = label,
                    fsize = file_size / 1048576,
                    ckb = chunk_kb,
                    clen = chunks.len(),
                    sb = search_bytes,
                    ratio = ratio,
                    avg_os = avg_overshoot,
                );
            }
        }
    }

    // Also measure pathological cases
    println!();
    println!("  --- Pathological cases ---");
    {
        // No delimiter at all
        let data = vec![b'x'; 1_048_576];
        let (chunks, sb) = find_chunk_boundaries_instrumented(&data, 65536, b'\n');
        println!(
            "  No delim 1MiB          {fsize:>10} MB {ckb:>8} KB {clen:<8} {sb:>10} B {ratio:>9.4}",
            fsize = 1,
            ckb = 64,
            clen = chunks.len(),
            sb = sb,
            ratio = sb as f64 / 1048576.0,
        );

        // chunk_size = 1 (worst case)
        let data = DataGenerator::new(100, b'\n').generate(1_048_576);
        let (chunks, sb) = find_chunk_boundaries_instrumented(&data, 1, b'\n');
        println!(
            "  chunk=1 JSONL 1MiB     {fsize:>10} MB {ckb:>8} KB {clen:<8} {sb:>10} B {ratio:>9.4}",
            fsize = 1,
            ckb = 1,
            clen = chunks.len(),
            sb = sb,
            ratio = sb as f64 / 1048576.0,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 10+11: END-TO-END SCANNER BENCHMARK (scalar vs SWAR)
// ═══════════════════════════════════════════════════════════════════════════════

/// find_chunk_boundaries variant using SWAR for delimiter search.
fn find_chunk_boundaries_swar(
    data: &[u8],
    chunk_size: usize,
    delimiter: u8,
) -> Vec<(usize, usize)> {
    if data.is_empty() {
        return Vec::new();
    }

    let len = data.len();
    let step = chunk_size.max(1);
    let estimate = (len / step) + 2;
    let mut chunks = Vec::with_capacity(estimate);
    let mut start = 0usize;

    while start < len {
        let mut end = start + step;
        if end >= len {
            end = len;
        } else {
            let remainder = &data[end..];
            if let Some(rel_pos) = find_byte_swar(remainder, delimiter) {
                end = end + rel_pos + 1;
                if end > len {
                    end = len;
                }
            } else {
                end = len;
            }
        }
        chunks.push((start, end));
        start = end;
    }

    chunks
}

fn bench_scanner<F>(data: &[u8], chunk_size: usize, delim: u8, f: F, iters: u64) -> (f64, usize)
where
    F: Fn(&[u8], usize, u8) -> Vec<(usize, usize)>,
{
    let start = Instant::now();
    let mut count = 0;
    for _ in 0..iters {
        let chunks = f(black_box(data), chunk_size, delim);
        count = chunks.len();
        black_box(&chunks);
    }
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    (elapsed_ms / iters as f64, count)
}

#[test]
#[ignore = "performance experiment — run with --ignored --nocapture"]
fn bench_scanner_end_to_end() {
    println!();
    println!("=== End-to-End Scanner Benchmark (scalar vs SWAR) ===");
    println!("  Dataset                    Size       Chunk KB   Scalar ms     SWAR ms      Ratio   Chunks");

    let delim = b'\n';
    let file_sizes: &[usize] = &[1_048_576, 16_777_216, 67_108_864];
    let chunk_sizes_kb: &[usize] = &[64, 256, 1024];

    let workload_configs: &[(&str, usize)] = &[
        ("JSONL ~100B", 100),
        ("Log ~300B", 300),
        ("Sparse ~64KiB", 65536),
    ];

    for &(wl_label, record_size) in workload_configs {
        let gen = DataGenerator::new(record_size, delim);
        for &file_size in file_sizes {
            let data = gen.generate(file_size);
            let file_mb = file_size / 1_048_576;

            for &chunk_kb in chunk_sizes_kb {
                let chunk_size = chunk_kb * 1024;

                // Warmup
                for _ in 0..3 {
                    let _ = black_box(find_chunk_boundaries(black_box(&data), chunk_size, delim));
                    let _ = black_box(find_chunk_boundaries_swar(
                        black_box(&data),
                        chunk_size,
                        delim,
                    ));
                }

                // Calibrate iterations
                let mut iters: u64 = 1;
                loop {
                    let start = Instant::now();
                    for _ in 0..iters {
                        black_box(find_chunk_boundaries(black_box(&data), chunk_size, delim));
                    }
                    let elapsed = start.elapsed().as_millis();
                    if elapsed > 200 || iters >= 100 {
                        break;
                    }
                    iters = (iters * 2).min(100);
                }

                let (s_ms, chunks) =
                    bench_scanner(&data, chunk_size, delim, find_chunk_boundaries, iters);
                let (w_ms, _) =
                    bench_scanner(&data, chunk_size, delim, find_chunk_boundaries_swar, iters);

                let ratio = if s_ms > 0.0 { w_ms / s_ms } else { f64::NAN };

                println!(
                    "  {wl:<22} {fmb:>8} MB {ckb:>8} KB {sms:>12.3} {wms:>12.3} {ratio:>9.3}x {chunks:>8}",
                    wl = wl_label,
                    fmb = file_mb,
                    ckb = chunk_kb,
                    sms = s_ms,
                    wms = w_ms,
                    ratio = ratio,
                    chunks = chunks,
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 12: memchr calibration (optional, requires `memchr` dev-dependency)
// ═══════════════════════════════════════════════════════════════════════════════

// #[cfg(feature = "memchr_ref")]
// mod memchr_calibration {
//     use super::*;
//
//     #[test]
//     #[ignore = "requires memchr dev-dependency — run with --ignored --nocapture"]
//     fn bench_memchr_calibration() {
//         println!();
//         println!("=== memchr Calibration ===");
//
//         let delim = b'\n';
//         let sizes: &[usize] = &[32, 64, 128, 256, 4096, 65536, 1048576];
//
//         println!("  Size         Scalar ns     SWAR ns     memchr ns");
//
//         for &size in sizes {
//             let data = vec![b'x'; size];
//             let s =
//                 bench_find_byte_auto(&data, delim, find_byte_scalar, &format!("scalar {size}B"));
//             let w = bench_find_byte_auto(&data, delim, find_byte_swar, &format!("SWAR   {size}B"));
//             let m = bench_find_byte_auto(
//                 &data,
//                 delim,
//                 |h, d| memchr::memchr(d, h),
//                 &format!("memchr {size}B"),
//             );
//             println!("  {size:>12} {s:>14.1} {w:>14.1} {m:>14.1}");
//         }
//     }
// }

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 10b: Property tests — SWAR scanner == scalar scanner
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod scanner_properties {
    use super::*;
    use mmap_chunker_core::scanner::find_chunk_boundaries;

    #[test]
    fn swar_scanner_equals_scalar_scanner() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"", 1024, b'\n'),
            (b"\n", 1, b'\n'),
            (b"hello\nworld\n", 4, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"one\ttwo\tthree", 4, b'\t'),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"x\x00y\x00z", 2, 0x00),
            (b"single", 1024, b'\n'),
            (b"\n\n\n\n\n", 1, b'\n'),
            (b"no_delim", 5, b'\n'),
            (b"line1\nline2\nline3\n", 2, b'\n'),
            (b"short\nverylongline\nshort\n", 6, b'\n'),
            (b"a\nb\nc\n", 1, b'\n'),
            (b"x", 1024, b'\n'),
            (b"a\nb\nc\nd\ne\n", 4, b'\n'),
        ];

        for &(data, chunk_size, delim) in cases {
            let original = find_chunk_boundaries(data, chunk_size, delim);
            let swar = find_chunk_boundaries_swar(data, chunk_size, delim);
            assert_eq!(
                original,
                swar,
                "SWAR chunker differs: len={} chunk_size={chunk_size} delim={delim:02x}",
                data.len()
            );
            // Additional property: total bytes covered must equal input length
            let orig_total: usize = original.iter().map(|(s, e)| e - s).sum();
            let swar_total: usize = swar.iter().map(|(s, e)| e - s).sum();
            assert_eq!(orig_total, data.len());
            assert_eq!(swar_total, data.len());
        }
    }
}
