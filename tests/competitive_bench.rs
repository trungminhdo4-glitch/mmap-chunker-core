//! Competitive performance benchmarks: mmap-chunker-core vs best-in-class.
//!
//! Run: cargo test --release --test competitive_bench -- --ignored --nocapture
//!
//! Methodology:
//! - Release build only (enforced)
//! - black_box on all inputs/outputs
//! - Multiple samples (p50, p10, p90 reported)
//! - Equivalent work verified before timing
//! - Warm OS-cached benchmarks (no cold-cache control)
//!
//! Lanes:
//!   A — single-byte search throughput (SWAR vs memchr vs scalar)
//!   B — multi-byte pattern search timing
//!   C — chunking workloads (eager, lazy, TFC, full traversal)
//!   D — partition quality
//!   E — mmap vs read
//!   F — scan amplification

use std::hint::black_box;
use std::time::Instant;

// ─── Statistics helpers ────────────────────────────────────────────────────

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

fn percentile(samples: &[f64], p: f64) -> f64 {
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() - 1) as f64 * p) as usize;
    v[idx]
}

fn elapsed_per_iter(iters: u64, mut f: impl FnMut()) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

fn bench_print(label: &str, ns: &[f64], total_bytes: f64) {
    let p50 = median(ns.to_vec());
    println!(
        "  {:45} p50={:>10.1}ns  p10={:>10.1}  p90={:>10.1}  {:>8.2} GiB/s",
        label,
        p50,
        percentile(ns, 0.10),
        percentile(ns, 0.90),
        total_bytes / p50 * 1e9 / (1 << 30) as f64,
    );
}

// ─── Test data generators ──────────────────────────────────────────────────

fn make_jsonl(target_size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(target_size);
    let mut n = 0u64;
    while data.len() < target_size {
        let line = format!(
            r#"{{"id":{},"ts":"2026-01-01T00:00:00Z","status":200,"msg":"ok","latency":{}}}"#,
            n,
            n % 100
        );
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
        n += 1;
    }
    data.truncate(target_size);
    data
}

fn make_logs(target_size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(target_size);
    let mut n = 0u64;
    while data.len() < target_size {
        let line = format!(
            "[2026-08-08T12:00:00Z] INFO request_id={} status=200 latency_ms={} endpoint=/api/v1/users\n",
            n, n % 100
        );
        data.extend_from_slice(line.as_bytes());
        n += 1;
    }
    data.truncate(target_size);
    data
}

fn make_fixed_records(record_size: usize, count: usize, delimiter: u8) -> Vec<u8> {
    let mut data = Vec::with_capacity((record_size + 1) * count);
    for _ in 0..count {
        data.extend_from_slice(&vec![b'A'; record_size]);
        data.push(delimiter);
    }
    data
}

fn make_crlf_data(target_size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(target_size);
    let mut n = 0u64;
    while data.len() < target_size {
        let line = format!("record_{}\r\n", n);
        data.extend_from_slice(line.as_bytes());
        n += 1;
    }
    data.truncate(target_size);
    data
}

fn make_adversarial_repeated_prefix(target_size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(target_size);
    while data.len() < target_size {
        data.extend_from_slice(b"aaaaaaaaaaa");
    }
    data.truncate(target_size);
    data
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane A — Single-byte search throughput: SWAR vs memchr vs scalar
//
// Measured via controlled scanning pipelines. Since find_byte_swar is
// pub(crate), we measure it indirectly through chunk-boundary scans
// where SWAR is the dominant work (delimiter-absent data).
// ═══════════════════════════════════════════════════════════════════════════

/// memchr-backed equivalent of our chunk boundary scanner (reference impl).
fn find_chunk_boundaries_memchr(
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
            if let Some(rel_pos) = memchr::memchr(delimiter, remainder) {
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

/// Scalar-equivalent reference implementation.
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

fn bench_lane_a_single_byte() {
    println!("\n═══ Lane A: Single-byte search (via chunk-boundary scan) ═══");

    let sizes: &[(usize, &str)] = &[(4096, "4 KiB"), (65536, "64 KiB"), (1_048_576, "1 MiB")];
    let sample_count = 11;

    for &(size, label) in sizes {
        // Data with dense delimiters: many short SWAR calls (realistic workload)
        let dense_data = make_jsonl(size);
        let delim = b'\n';
        let chunk_size = 65536;

        // Verify equivalence
        let our = mmap_chunker_core::scanner::find_chunk_boundaries(&dense_data, chunk_size, delim);
        let mem = find_chunk_boundaries_memchr(&dense_data, chunk_size, delim);
        let scl = find_chunk_boundaries_scalar(&dense_data, chunk_size, delim);
        assert_eq!(our, mem, "SWAR vs memchr mismatch at {label}");
        assert_eq!(our, scl, "SWAR vs scalar mismatch at {label}");

        let iters = if size <= 4096 {
            2000u64
        } else if size <= 65536 {
            200
        } else {
            30
        };

        // Dense workload — many short SWAR calls
        println!("\n  --- delimited {label} (dense -- realistic) ---");
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&dense_data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let result = mmap_chunker_core::scanner::find_chunk_boundaries(d, cs, dl);
                    black_box(result);
                }));
            }
            bench_print("our SWAR   dense", &ns, size as f64 * iters as f64);
        }
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&dense_data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let result = find_chunk_boundaries_memchr(d, cs, dl);
                    black_box(result);
                }));
            }
            bench_print("memchr     dense", &ns, size as f64 * iters as f64);
        }
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&dense_data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let result = find_chunk_boundaries_scalar(d, cs, dl);
                    black_box(result);
                }));
            }
            bench_print("scalar     dense", &ns, size as f64 * iters as f64);
        }

        // No-delimiter data: one large SWAR call (worst-case for search)
        let nodata = vec![b'x'; size];
        // Verify: same output
        let our_n = mmap_chunker_core::scanner::find_chunk_boundaries(&nodata, chunk_size, delim);
        let mem_n = find_chunk_boundaries_memchr(&nodata, chunk_size, delim);
        assert_eq!(our_n, mem_n, "SWAR vs memchr no-delim mismatch at {label}");
        assert_eq!(
            our_n,
            find_chunk_boundaries_scalar(&nodata, chunk_size, delim)
        );

        println!("\n  --- no-delimiter {label} (full scan -- worst-case) ---");
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&nodata);
                    let result = mmap_chunker_core::scanner::find_chunk_boundaries(
                        d,
                        black_box(chunk_size),
                        black_box(delim),
                    );
                    black_box(result);
                }));
            }
            bench_print("our SWAR   absent", &ns, size as f64 * iters as f64);
        }
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&nodata);
                    let result =
                        find_chunk_boundaries_memchr(d, black_box(chunk_size), black_box(delim));
                    black_box(result);
                }));
            }
            bench_print("memchr     absent", &ns, size as f64 * iters as f64);
        }
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&nodata);
                    let result =
                        find_chunk_boundaries_scalar(d, black_box(chunk_size), black_box(delim));
                    black_box(result);
                }));
            }
            bench_print("scalar     absent", &ns, size as f64 * iters as f64);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane B — Multi-byte pattern search
// ═══════════════════════════════════════════════════════════════════════════

fn find_pattern_boundaries_memmem(
    data: &[u8],
    chunk_size: usize,
    pattern: &[u8],
) -> Vec<(usize, usize)> {
    if data.is_empty() {
        return Vec::new();
    }
    let finder = memchr::memmem::Finder::new(pattern);
    let dlen = pattern.len();
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
            if let Some(rel_pos) = finder.find(remainder) {
                end = end + rel_pos + dlen;
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

fn bench_lane_b_multi_byte() {
    println!("\n═══ Lane B: Multi-byte pattern search ═══");

    let workloads: &[(&[u8], usize, &[u8], &str)] = &[
        (
            b"record_123\r\nrecord_456\r\nrecord_789\r\n",
            4096,
            b"\r\n",
            "CRLF dense 4 KiB",
        ),
        (
            b"header1=val1\r\n\r\nheader2=val2\r\n\r\nbody_start\r\n\r\n",
            4096,
            b"\r\n\r\n",
            "double CRLF HTTP 4 KiB",
        ),
        (
            b"col1||col2||col3||col4||col5||",
            4096,
            b"||",
            "pipe double 4 KiB",
        ),
        (
            b"END_RECORD\x00dataEND_RECORD\x00more",
            4096,
            b"END_RECORD",
            "named 11B 4 KiB",
        ),
    ];

    let sample_count = 11;

    for &(seed, multiplier, pattern, label) in workloads {
        let mut data = seed.repeat(multiplier / seed.len().max(1) + 1);
        data.truncate(multiplier);

        let size = data.len();
        let iters = if size <= 512 {
            2000u64
        } else if size <= 4096 {
            500
        } else {
            100
        };

        // Verify equivalence
        let our =
            mmap_chunker_core::scanner::find_chunk_boundaries_pattern(&data, 64 * 1024, pattern);
        let mem = find_pattern_boundaries_memmem(&data, 64 * 1024, pattern);
        assert_eq!(our, mem, "pattern boundary mismatch: {label}");

        println!("\n  --- {} ---", label);
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&data);
                    let p = black_box(pattern);
                    let result = mmap_chunker_core::scanner::find_chunk_boundaries_pattern(
                        d,
                        black_box(64 * 1024),
                        p,
                    );
                    black_box(result);
                }));
            }
            bench_print("our pattern", &ns, size as f64 * iters as f64);
        }
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&data);
                    let p = black_box(pattern);
                    let result = find_pattern_boundaries_memmem(d, black_box(64 * 1024), p);
                    black_box(result);
                }));
            }
            bench_print("memmem finder", &ns, size as f64 * iters as f64);
        }
    }

    // Adversarial: "aaaaab" in "aaaaaa..."
    println!("\n  --- Adversarial: 'aaaaab' in repeated 'a' ---");
    let sizes_adv: &[(usize, &str)] = &[(256, "256 B"), (4096, "4 KiB"), (65536, "64 KiB")];
    for &(size, label) in sizes_adv {
        let data = make_adversarial_repeated_prefix(size);
        let pattern: &[u8] = b"aaaaab";

        let our =
            mmap_chunker_core::scanner::find_chunk_boundaries_pattern(&data, size / 4, pattern);
        let mem = find_pattern_boundaries_memmem(&data, size / 4, pattern);
        assert_eq!(our, mem, "adversarial mismatch at {label}");

        let iters = if size <= 256 {
            200u64
        } else if size <= 4096 {
            50
        } else {
            5
        };
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&data);
                    let result = mmap_chunker_core::scanner::find_chunk_boundaries_pattern(
                        d,
                        black_box(size / 4),
                        black_box(b"aaaaab"),
                    );
                    black_box(result);
                }));
            }
            bench_print(
                &format!("our adv    {label}"),
                &ns,
                size as f64 * iters as f64,
            );
        }
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(&data);
                    let result = find_pattern_boundaries_memmem(
                        d,
                        black_box(size / 4),
                        black_box(b"aaaaab"),
                    );
                    black_box(result);
                }));
            }
            bench_print(
                &format!("memmem adv {label}"),
                &ns,
                size as f64 * iters as f64,
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane C — Chunking workloads
// ═══════════════════════════════════════════════════════════════════════════

fn bench_lane_c_chunking() {
    println!("\n═══ Lane C: Chunking workloads ═══");

    let workloads: &[(&str, Vec<u8>, usize, u8, u64)] = &[
        (
            "JSONL ~100B rec 1 MiB   64KiB",
            make_jsonl(1_048_576),
            65536,
            b'\n',
            100,
        ),
        (
            "JSONL ~100B rec 16 MiB  64KiB",
            make_jsonl(16_777_216),
            65536,
            b'\n',
            20,
        ),
        (
            "Logs  ~300B rec 1 MiB   64KiB",
            make_logs(1_048_576),
            65536,
            b'\n',
            50,
        ),
        (
            "Logs  ~300B rec 16 MiB  1MiB",
            make_logs(16_777_216),
            1_048_576,
            b'\n',
            10,
        ),
        (
            "CRLF  ~100B rec 1 MiB   64KiB",
            make_crlf_data(1_048_576),
            65536,
            b'\r',
            50,
        ),
    ];

    let sample_count = 9;

    for (label, data, chunk_size, delim, iters) in workloads {
        let size = data.len();

        let eager_chunks =
            mmap_chunker_core::scanner::find_chunk_boundaries(data, *chunk_size, *delim).len();
        let lazy_chunks =
            mmap_chunker_core::scanner::ChunkCursor::new(data, *chunk_size, *delim).count();
        assert_eq!(eager_chunks, lazy_chunks, "eager vs lazy mismatch: {label}");

        println!("\n  --- {label} ({eager_chunks} chunks) ---");

        let labels = ["TFC eager", "TFC lazy ", "Full eager", "Full lazy "];
        let mut all_ns: Vec<Vec<f64>> = vec![Vec::new(); 4];

        for _ in 0..sample_count {
            all_ns[0].push(elapsed_per_iter(*iters, || {
                let d = black_box(&data);
                let chunks = mmap_chunker_core::scanner::find_chunk_boundaries(
                    d,
                    black_box(*chunk_size),
                    black_box(*delim),
                );
                black_box(chunks.first().copied());
            }));
            all_ns[1].push(elapsed_per_iter(*iters, || {
                let d = black_box(&data);
                let mut c = mmap_chunker_core::scanner::ChunkCursor::new(
                    d,
                    black_box(*chunk_size),
                    black_box(*delim),
                );
                black_box(c.next());
            }));
            all_ns[2].push(elapsed_per_iter(*iters, || {
                let d = black_box(&data);
                let chunks = mmap_chunker_core::scanner::find_chunk_boundaries(
                    d,
                    black_box(*chunk_size),
                    black_box(*delim),
                );
                let mut total = 0usize;
                for &(s, e) in &chunks {
                    total = total.wrapping_add(e - s);
                }
                black_box(total);
                black_box(chunks);
            }));
            all_ns[3].push(elapsed_per_iter(*iters, || {
                let d = black_box(&data);
                let mut total = 0usize;
                for chunk in mmap_chunker_core::scanner::ChunkCursor::new(
                    d,
                    black_box(*chunk_size),
                    black_box(*delim),
                ) {
                    total = total.wrapping_add(chunk.len());
                }
                black_box(total);
            }));
        }

        let total_bytes = size as f64 * *iters as f64;
        for (i, ns) in all_ns.iter().enumerate() {
            bench_print(labels[i], ns, total_bytes);
        }
        println!(
            "  metadata: {} bytes ({} chunks x 16B)",
            16 * eager_chunks,
            eager_chunks
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane D — Partition quality
// ═══════════════════════════════════════════════════════════════════════════

fn bench_lane_d_partition_quality() {
    println!("\n═══ Lane D: Partition quality ═══");

    let scenarios: &[(&str, Vec<u8>)] = &[
        ("fixed 100B x 1000", make_fixed_records(100, 1000, b'\n')),
        ("variable 30-300B x 1000", {
            let mut data = Vec::new();
            for i in 0..1000 {
                let sz = 30 + (i * 7 % 270);
                data.extend_from_slice(&vec![b'A'; sz]);
                data.push(b'\n');
            }
            data
        }),
        ("skewed 100B x 99 + giant 100KB", {
            let mut data = Vec::new();
            for _ in 0..99 {
                data.extend_from_slice(&[b'A'; 100]);
                data.push(b'\n');
            }
            data.extend_from_slice(&vec![b'X'; 100_000]);
            data.push(b'\n');
            data
        }),
    ];

    for (label, data) in scenarios {
        for &n in &[2, 4, 8, 16] {
            let partitions = mmap_chunker_core::scanner::find_partition_boundaries(data, n, b'\n');
            let actual_n = partitions.len();
            if actual_n == 0 {
                continue;
            }

            let sizes: Vec<usize> = partitions.iter().map(|(s, e)| e - s).collect();
            let min_sz = sizes.iter().min().unwrap();
            let max_sz = sizes.iter().max().unwrap();
            let mean_sz = sizes.iter().sum::<usize>() as f64 / actual_n as f64;
            let ratio = *max_sz as f64 / mean_sz;

            let all_ends_ok = partitions
                .iter()
                .all(|(_, e)| *e == data.len() || data.get(*e - 1) == Some(&b'\n'));
            let concat_total: usize = sizes.iter().sum();
            assert_eq!(concat_total, data.len(), "coverage mismatch {label} N={n}");
            assert!(all_ends_ok, "record split in {label} N={n}");

            println!("  {label:35} N={n:2} -> {actual_n:2} parts  min={min_sz:>10}  max={max_sz:>10}  mean={mean_sz:>10.0}  max/mean={ratio:.2}");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane E — mmap vs read (I/O baseline, warm cache)
// ═══════════════════════════════════════════════════════════════════════════

fn bench_lane_e_mmap_vs_read() {
    println!("\n═══ Lane E: mmap vs read (WARM / OS-cached benchmark) ═══");

    let file_sizes_mb: [(usize, usize); 2] = [(1, 64), (16, 64)];
    let sample_count = 7;

    for (size_mb, chunk_size_kb) in &file_sizes_mb {
        let target = size_mb * 1024 * 1024;
        let chunk_size = chunk_size_kb * 1024;

        let dir = std::env::temp_dir().join("mmap_bench_e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("bench_{}mb.dat", size_mb));
        let content = b"[2026-08-08] record_000000 status=ok latency=42ms payload=test_data_here\n"
            .repeat(target / 80);
        std::fs::write(&path, &content).unwrap();

        // Warm
        let _ = std::fs::read(&path).unwrap();
        let chunk_count =
            mmap_chunker_core::scanner::find_chunk_boundaries(&content, chunk_size, b'\n').len();

        // mmap path: open + scan + free
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(1, || {
                    let mmap = unsafe {
                        mmap_chunker_core::mmap::MmapFile::open_path(black_box(&path)).unwrap()
                    };
                    let data = mmap.as_bytes();
                    let chunks = mmap_chunker_core::scanner::find_chunk_boundaries(
                        data,
                        black_box(chunk_size),
                        b'\n',
                    );
                    let mut total: usize = 0;
                    for &(s, e) in &chunks {
                        total = total.wrapping_add(e - s);
                    }
                    black_box(total);
                }));
            }
            let p50 = median(ns.clone());
            println!(
                "  mmap+scan  {}MB {}KB  p50={:>8.1}μs  p10={:>8.1}  p90={:>8.1}  chunks={}",
                size_mb,
                chunk_size_kb,
                p50 / 1000.0,
                percentile(&ns, 0.10) / 1000.0,
                percentile(&ns, 0.90) / 1000.0,
                chunk_count,
            );
        }

        // fs::read path
        {
            let mut ns = Vec::with_capacity(sample_count);
            for _ in 0..sample_count {
                ns.push(elapsed_per_iter(1, || {
                    let data = std::fs::read(black_box(&path)).unwrap();
                    let chunks = mmap_chunker_core::scanner::find_chunk_boundaries(
                        &data,
                        black_box(chunk_size),
                        b'\n',
                    );
                    let mut total: usize = 0;
                    for &(s, e) in &chunks {
                        total = total.wrapping_add(e - s);
                    }
                    black_box(total);
                }));
            }
            let p50 = median(ns.clone());
            println!(
                "  fs::read   {}MB {}KB  p50={:>8.1}μs  p10={:>8.1}  p90={:>8.1}",
                size_mb,
                chunk_size_kb,
                p50 / 1000.0,
                percentile(&ns, 0.10) / 1000.0,
                percentile(&ns, 0.90) / 1000.0,
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane F — Scan amplification: how much data is actually scanned
// ═══════════════════════════════════════════════════════════════════════════

fn bench_lane_f_scan_amplification() {
    println!("\n═══ Lane F: Scan amplification ═══");

    let scenarios: &[(usize, usize, &str)] = &[
        (1_048_576, 65536, "1 MiB, 64 KiB chunk"),
        (16_777_216, 65536, "16 MiB, 64 KiB chunk"),
        (16_777_216, 1_048_576, "16 MiB, 1 MiB chunk"),
    ];

    for &(size, chunk_size, label) in scenarios {
        let data = make_jsonl(size);
        let chunks = mmap_chunker_core::scanner::find_chunk_boundaries(&data, chunk_size, b'\n');
        let metadata_bytes = 16 * chunks.len();
        let avg_search_window: f64 = chunks
            .iter()
            .map(|(s, e)| {
                let target = s + chunk_size;
                if target >= data.len() {
                    0
                } else {
                    e.saturating_sub(target)
                }
            })
            .filter(|&w| w > 0)
            .sum::<usize>() as f64
            / chunks.len().max(1) as f64;

        let total_scanned = chunks
            .iter()
            .map(|(s, e)| {
                let target = s + chunk_size;
                if target >= data.len() {
                    0usize
                } else {
                    (e - 1).saturating_sub(target) + 1
                }
            })
            .sum::<usize>();

        println!("  {label:30} chunks={:>6}  metadata={:>8}B  avg_search_win={:>6.0}B  scanned={}/{} ({:.3}%)",
            chunks.len(), metadata_bytes, avg_search_window,
            total_scanned, size,
            total_scanned as f64 / size as f64 * 100.0,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Main entry point
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore = "competitive benchmark — run with cargo test --release --test competitive_bench -- --ignored --nocapture"]
#[allow(clippy::assertions_on_constants)]
fn competitive_bench() {
    assert!(
        !cfg!(debug_assertions),
        "ERROR: competitive_bench requires --release build. Aborting."
    );

    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  mmap-chunker-core — Competitive Performance Benchmarks");
    println!("══════════════════════════════════════════════════════════════════");
    println!(
        "  Version: {} (package under test)",
        env!("CARGO_PKG_VERSION")
    );
    println!("  MSRV:    1.77");
    println!(
        "  OS:      {}  Arch: {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "  CPU:     {} logical cores",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    println!(
        "  Build:   {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("  Toolchain: {}", rustc_version());
    println!("══════════════════════════════════════════════════════════════════");

    bench_lane_a_single_byte();
    bench_lane_b_multi_byte();
    bench_lane_c_chunking();
    bench_lane_d_partition_quality();
    bench_lane_e_mmap_vs_read();
    bench_lane_f_scan_amplification();

    println!("\n═══ Complete ═══\n");
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}
