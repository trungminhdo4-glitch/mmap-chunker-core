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
/// by the delimiter search (early-exit semantics).
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
            if let Some(rel_pos) = remainder.iter().position(|&b| b == delimiter) {
                search_bytes += rel_pos + 1;
                end = end + rel_pos + 1;
                if end > len {
                    end = len;
                }
            } else {
                search_bytes += remainder.len();
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
        "  Dataset                  File MB   Chk KB  Chunks  Searches   ExmB   Exm/Inp  MeanExm   p50OS   p95OS  maxOS"
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
                let (chunks, exm_bytes) =
                    find_chunk_boundaries_instrumented(&data, chunk_size, b'\n');

                let searches = chunks.len().saturating_sub(1);

                let ratio = if file_size > 0 {
                    exm_bytes as f64 / file_size as f64
                } else {
                    0.0
                };

                let mean_exm = if searches > 0 {
                    exm_bytes as f64 / searches as f64
                } else {
                    0.0
                };

                let mut overshoots: Vec<usize> = chunks
                    .iter()
                    .map(|(s, e)| (e - s).saturating_sub(chunk_size.min(e - s)))
                    .collect();
                overshoots.sort_unstable();
                let n = overshoots.len();
                let p50 = if n > 0 { overshoots[n / 2] } else { 0 };
                let p95_idx = if n > 0 {
                    ((n - 1) as f64 * 0.95) as usize
                } else {
                    0
                };
                let p95 = if n > 0 { overshoots[p95_idx] } else { 0 };
                let max_os = overshoots.last().copied().unwrap_or(0);

                println!(
                    "  {label:<20} {fsize:>8} {ckb:>7} {clen:>7} {srch:>9} {exmb:>7} {ratio:>9.4} {mexm:>9.1} {p50:>7} {p95:>7} {mos:>7}",
                    label = label,
                    fsize = file_size / 1048576,
                    ckb = chunk_kb,
                    clen = chunks.len(),
                    srch = searches,
                    exmb = exm_bytes,
                    ratio = ratio,
                    mexm = mean_exm,
                    p50 = p50,
                    p95 = p95,
                    mos = max_os,
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
        let (chunks, exm) = find_chunk_boundaries_instrumented(&data, 65536, b'\n');
        let searches = chunks.len().saturating_sub(1);
        println!(
            "  No delim 1MiB          {fsize:>8} {ckb:>7} {clen:>7} {srch:>9} {exmb:>7} {ratio:>9.4}",
            fsize = 1,
            ckb = 64,
            clen = chunks.len(),
            srch = searches,
            exmb = exm,
            ratio = exm as f64 / 1048576.0,
        );

        // chunk_size = 1 (worst case)
        let data = DataGenerator::new(100, b'\n').generate(1_048_576);
        let (chunks, exm) = find_chunk_boundaries_instrumented(&data, 1, b'\n');
        let searches = chunks.len().saturating_sub(1);
        println!(
            "  chunk=1 JSONL 1MiB     {fsize:>8} {ckb:>7} {clen:>7} {srch:>9} {exmb:>7} {ratio:>9.4}",
            fsize = 1,
            ckb = 1,
            clen = chunks.len(),
            srch = searches,
            exmb = exm,
            ratio = exm as f64 / 1048576.0,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 10+11: SCANNER BENCHMARK — production (SWAR) vs scalar baseline
// ═══════════════════════════════════════════════════════════════════════════════

/// Genuine scalar variant of find_chunk_boundaries for baseline comparison.
/// Uses `position()` for delimiter search — no SWAR, no unsafe.
fn find_chunk_boundaries_scalar(
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
    println!("=== Scanner Benchmark (production SWAR vs scalar baseline) ===");
    println!(
        "  Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("  Dataset                    Size       Chunk KB   Prod SWAR ms  Scalar ms    Ratio   Chunks");

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
                    let _ = black_box(find_chunk_boundaries_scalar(
                        black_box(&data),
                        chunk_size,
                        delim,
                    ));
                }

                // Calibrate iterations using production path
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
                let (w_ms, _) = bench_scanner(
                    &data,
                    chunk_size,
                    delim,
                    find_chunk_boundaries_scalar,
                    iters,
                );

                let ratio = if w_ms > 0.0 { s_ms / w_ms } else { f64::NAN };

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
            let scalar = find_chunk_boundaries_scalar(data, chunk_size, delim);
            assert_eq!(
                original,
                scalar,
                "production != scalar: len={} chunk_size={chunk_size} delim={delim:02x}",
                data.len()
            );
            // Additional property: total bytes covered must equal input length
            let orig_total: usize = original.iter().map(|(s, e)| e - s).sum();
            let scalar_total: usize = scalar.iter().map(|(s, e)| e - s).sum();
            assert_eq!(orig_total, data.len());
            assert_eq!(scalar_total, data.len());
        }
    }
}
