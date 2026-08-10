//! Small, reproducible performance foundation for the source-independent scanner core.
//!
//! Run the Tier-1 baseline with:
//!
//! ```text
//! cargo test --release --test benchmark_foundation -- --ignored --nocapture
//! ```
//!
//! Tier 2/3 add only the selected larger workloads:
//!
//! ```text
//! MMAP_BENCH_TIER=2 cargo test --release --test benchmark_foundation -- --ignored --nocapture
//! ```
//!
//! This is deliberately a small `std` harness. It is evidence collection, not a
//! replacement benchmark framework and not a production optimization.

use memchr::{memchr, memmem};
use mmap_chunker_core::mmap::MmapFile;
use mmap_chunker_core::scanner::{
    find_chunk_boundaries, find_chunk_boundaries_pattern, find_partition_boundaries, ChunkCursor,
    PatternChunkCursor,
};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

const WARMUP_COUNT: usize = 4;
const SAMPLE_COUNT: usize = 15;
const TIER_1_SIZES: &[usize] = &[4 * 1024, 64 * 1024, 1024 * 1024];
const TIER_2_SIZES: &[usize] = &[4 * 1024, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024];
const TIER_3_SIZES: &[usize] = &[
    4 * 1024,
    64 * 1024,
    1024 * 1024,
    16 * 1024 * 1024,
    64 * 1024 * 1024,
];

#[derive(Debug)]
struct BenchmarkStats {
    min_ns: f64,
    max_ns: f64,
    mean_ns: f64,
    p10_ns: f64,
    p50_ns: f64,
    p90_ns: f64,
    p95_ns: f64,
    relative_spread_percent: f64,
}

#[derive(Debug)]
struct SingleWorkload {
    name: &'static str,
    data: Vec<u8>,
    delimiter: u8,
    chunk_size: usize,
}

#[derive(Debug)]
struct PatternWorkload {
    name: &'static str,
    data: Vec<u8>,
    pattern: &'static [u8],
    chunk_size: usize,
}

#[derive(Debug)]
struct PartitionWorkload {
    name: &'static str,
    data: Vec<u8>,
}

fn percentile(samples: &[f64], p: f64) -> f64 {
    assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let index = ((sorted.len() - 1) as f64 * p) as usize;
    sorted[index]
}

fn summarize(samples: &[f64]) -> BenchmarkStats {
    assert!(!samples.is_empty());
    let min_ns = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let max_ns = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mean_ns = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50_ns = percentile(samples, 0.50);

    BenchmarkStats {
        min_ns,
        max_ns,
        mean_ns,
        p10_ns: percentile(samples, 0.10),
        p50_ns,
        p90_ns: percentile(samples, 0.90),
        p95_ns: percentile(samples, 0.95),
        relative_spread_percent: if p50_ns > 0.0 {
            (max_ns - min_ns) / p50_ns * 100.0
        } else {
            0.0
        },
    }
}

fn elapsed_per_iter_ns(iters: usize, mut operation: impl FnMut()) -> f64 {
    assert!(iters > 0);
    let start = Instant::now();
    for _ in 0..iters {
        operation();
    }
    start.elapsed().as_secs_f64() * 1e9 / iters as f64
}

fn sample_one(mut operation: impl FnMut(), iters: usize) -> Vec<f64> {
    for _ in 0..WARMUP_COUNT {
        operation();
    }

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        samples.push(elapsed_per_iter_ns(iters, &mut operation));
    }
    samples
}

/// Measure two alternatives with alternating order to make same-process order bias visible.
fn sample_pair(
    mut left: impl FnMut(),
    mut right: impl FnMut(),
    iters: usize,
) -> (Vec<f64>, Vec<f64>) {
    for index in 0..WARMUP_COUNT {
        if index % 2 == 0 {
            left();
            right();
        } else {
            right();
            left();
        }
    }

    let mut left_samples = Vec::with_capacity(SAMPLE_COUNT);
    let mut right_samples = Vec::with_capacity(SAMPLE_COUNT);
    for index in 0..SAMPLE_COUNT {
        if index % 2 == 0 {
            left_samples.push(elapsed_per_iter_ns(iters, &mut left));
            right_samples.push(elapsed_per_iter_ns(iters, &mut right));
        } else {
            right_samples.push(elapsed_per_iter_ns(iters, &mut right));
            left_samples.push(elapsed_per_iter_ns(iters, &mut left));
        }
    }
    (left_samples, right_samples)
}

fn throughput_bytes_per_second(bytes_processed: f64, elapsed_seconds: f64) -> Option<f64> {
    if bytes_processed <= 0.0 || elapsed_seconds <= 0.0 {
        None
    } else {
        Some(bytes_processed / elapsed_seconds)
    }
}

fn format_size(bytes: Option<usize>) -> String {
    match bytes {
        Some(value) => value.to_string(),
        None => "n/a".to_string(),
    }
}

fn format_throughput(bytes_per_iter: Option<usize>, p50_ns: f64) -> String {
    match bytes_per_iter.and_then(|bytes| throughput_bytes_per_second(bytes as f64, p50_ns * 1e-9))
    {
        Some(bytes_per_second) => format!("{:.3}GiB/s", bytes_per_second / (1 << 30) as f64),
        None => "n/a".to_string(),
    }
}

fn print_result(
    category: &str,
    workload: &str,
    size: usize,
    params: &str,
    bytes_per_iter: Option<usize>,
    iters: usize,
    stats: &BenchmarkStats,
) {
    println!(
        "{category} workload={workload} size={size}B samples={SAMPLE_COUNT} warmups={WARMUP_COUNT} batch_iters={iters} p10={:.1}ns p50={:.1}ns p90={:.1}ns p95={:.1}ns mean={:.1}ns min={:.1}ns max={:.1}ns spread={:.2}% bytes_processed={} throughput={} params={params}",
        stats.p10_ns,
        stats.p50_ns,
        stats.p90_ns,
        stats.p95_ns,
        stats.mean_ns,
        stats.min_ns,
        stats.max_ns,
        stats.relative_spread_percent,
        format_size(bytes_per_iter),
        format_throughput(bytes_per_iter, stats.p50_ns),
    );
}

fn repeat_to_size(seed: &[u8], size: usize) -> Vec<u8> {
    assert!(!seed.is_empty());
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        let take = (size - data.len()).min(seed.len());
        data.extend_from_slice(&seed[..take]);
    }
    data
}

fn random_bytes(size: usize, mut state: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.push(state as u8);
    }
    data
}

fn chunk_size_for(size: usize) -> usize {
    if size <= 4 * 1024 {
        1024
    } else if size <= 64 * 1024 {
        16 * 1024
    } else {
        64 * 1024
    }
}

fn single_workloads(size: usize, tier: u8) -> Vec<SingleWorkload> {
    let sparse = {
        let mut data = vec![b'x'; size];
        if let Some(last) = data.last_mut() {
            *last = b'\n';
        }
        data
    };
    let mut workloads = vec![
        SingleWorkload {
            name: "newline_dense",
            data: repeat_to_size(b"short record\n", size),
            delimiter: b'\n',
            chunk_size: chunk_size_for(size),
        },
        SingleWorkload {
            name: "sparse_delimiter",
            data: sparse,
            delimiter: b'\n',
            chunk_size: chunk_size_for(size),
        },
        SingleWorkload {
            name: "delimiter_absent",
            data: vec![b'x'; size],
            delimiter: b'\n',
            chunk_size: chunk_size_for(size),
        },
        SingleWorkload {
            name: "delimiter_everywhere",
            data: vec![b'\n'; size],
            delimiter: b'\n',
            chunk_size: chunk_size_for(size),
        },
        SingleWorkload {
            name: "random_binary",
            data: random_bytes(size, 0x9e37_79b9_7f4a_7c15),
            delimiter: b'\n',
            chunk_size: chunk_size_for(size),
        },
    ];

    if tier >= 2 {
        workloads.retain(|workload| {
            matches!(
                workload.name,
                "newline_dense" | "sparse_delimiter" | "delimiter_absent" | "random_binary"
            )
        });
    }
    workloads
}

const CRLF: &[u8] = b"\r\n";
const DOUBLE_CRLF: &[u8] = b"\r\n\r\n";
const LONG_DELIMITER: &[u8] = b"0123456789ABCDEF";
const ABSENT_PATTERN: &[u8] = b"not-present-pattern";
const RANDOM_PATTERN: &[u8] = b"\xff\xff\xff\xff\xff\xff\xff\xff";
const ADVERSARIAL_PATTERN: &[u8] = b"aaaaab";

fn pattern_workloads(size: usize, tier: u8) -> Vec<PatternWorkload> {
    let mut workloads = vec![
        PatternWorkload {
            name: "crlf",
            data: repeat_to_size(b"record-000001\r\n", size),
            pattern: CRLF,
            chunk_size: chunk_size_for(size),
        },
        PatternWorkload {
            name: "four_byte_delimiter",
            data: repeat_to_size(b"record\r\n\r\n", size),
            pattern: DOUBLE_CRLF,
            chunk_size: chunk_size_for(size),
        },
        PatternWorkload {
            name: "long_delimiter_16B",
            data: repeat_to_size(b"record-0123456789ABCDEF", size),
            pattern: LONG_DELIMITER,
            chunk_size: chunk_size_for(size),
        },
        PatternWorkload {
            name: "pattern_absent",
            data: vec![b'x'; size],
            pattern: ABSENT_PATTERN,
            chunk_size: chunk_size_for(size),
        },
        PatternWorkload {
            name: "dense_matches",
            data: repeat_to_size(CRLF, size),
            pattern: CRLF,
            chunk_size: chunk_size_for(size),
        },
        PatternWorkload {
            name: "random_no_match",
            data: random_bytes(size, 0x243f_6a88_85a3_08d3),
            pattern: RANDOM_PATTERN,
            chunk_size: chunk_size_for(size),
        },
        PatternWorkload {
            name: "adversarial_repeated_prefix",
            data: vec![b'a'; size],
            pattern: ADVERSARIAL_PATTERN,
            chunk_size: chunk_size_for(size),
        },
    ];

    if tier >= 2 {
        workloads.retain(|workload| {
            matches!(
                workload.name,
                "crlf" | "pattern_absent" | "random_no_match" | "adversarial_repeated_prefix"
            )
        });
    }
    workloads
}

fn variable_record_data(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut record = 0usize;
    while data.len() < size {
        let record_size = 30 + (record * 17 % 270);
        let payload_size = (size - data.len()).min(record_size);
        data.extend(std::iter::repeat(b'A' + (record % 26) as u8).take(payload_size));
        if data.len() < size {
            data.push(b'\n');
        }
        record += 1;
    }
    data
}

fn skewed_record_data(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let prefix_target = size.saturating_mul(3) / 4;
    while data.len() + 101 < prefix_target {
        data.extend(std::iter::repeat(b'A').take(100));
        data.push(b'\n');
    }
    if data.len() < size {
        let payload_size = size - data.len();
        if payload_size > 1 {
            data.extend(std::iter::repeat(b'X').take(payload_size - 1));
            data.push(b'\n');
        } else {
            data.push(b'\n');
        }
    }
    data
}

fn uniform_record_data(size: usize) -> Vec<u8> {
    let mut record = vec![b'A'; 100];
    record.push(b'\n');
    repeat_to_size(&record, size)
}

fn partition_workloads(size: usize, tier: u8) -> Vec<PartitionWorkload> {
    let mut workloads = vec![
        PartitionWorkload {
            name: "partition_uniform_records",
            data: uniform_record_data(size),
        },
        PartitionWorkload {
            name: "partition_variable_records",
            data: variable_record_data(size),
        },
        PartitionWorkload {
            name: "partition_skewed_giant_tail",
            data: skewed_record_data(size),
        },
    ];

    // Tier 2/3 retain one realistic and one stress shape to keep larger runs bounded.
    if tier >= 2 {
        workloads.retain(|workload| {
            matches!(
                workload.name,
                "partition_uniform_records" | "partition_skewed_giant_tail"
            )
        });
    }
    workloads
}

fn iterations_for(size: usize) -> usize {
    if size <= 4 * 1024 {
        256
    } else if size <= 64 * 1024 {
        32
    } else if size <= 1024 * 1024 {
        4
    } else {
        1
    }
}

fn scan_single_memchr(data: &[u8], chunk_size: usize, delimiter: u8) -> Vec<(usize, usize)> {
    if data.is_empty() {
        return Vec::new();
    }

    let step = chunk_size.max(1);
    let mut chunks = Vec::with_capacity(data.len() / step + 2);
    let mut start = 0usize;
    while start < data.len() {
        let mut end = start.saturating_add(step);
        if end >= data.len() {
            end = data.len();
        } else if let Some(relative) = memchr(delimiter, &data[end..]) {
            end = end.saturating_add(relative + 1).min(data.len());
        } else {
            end = data.len();
        }
        chunks.push((start, end));
        start = end;
    }
    chunks
}

fn scan_pattern_memmem(data: &[u8], chunk_size: usize, pattern: &[u8]) -> Vec<(usize, usize)> {
    assert!(!pattern.is_empty());
    if data.is_empty() {
        return Vec::new();
    }

    let step = chunk_size.max(1);
    let mut chunks = Vec::with_capacity(data.len() / step + 2);
    let mut start = 0usize;
    while start < data.len() {
        let mut end = start.saturating_add(step);
        if end >= data.len() {
            end = data.len();
        } else if let Some(relative) = memmem::find(&data[end..], pattern) {
            end = end
                .saturating_add(relative)
                .saturating_add(pattern.len())
                .min(data.len());
        } else {
            end = data.len();
        }
        chunks.push((start, end));
        start = end;
    }
    chunks
}

fn metadata_bytes(range_count: usize) -> usize {
    range_count * std::mem::size_of::<(usize, usize)>()
}

fn assert_valid_ranges(data: &[u8], ranges: &[(usize, usize)]) {
    let mut cursor = 0usize;
    for &(start, end) in ranges {
        assert_eq!(start, cursor);
        assert!(end > start);
        assert!(end == data.len() || data[end - 1] == b'\n');
        cursor = end;
    }
    assert_eq!(cursor, data.len());
}

fn consume_cursor_single(data: &[u8], chunk_size: usize, delimiter: u8) {
    let data = black_box(data);
    let mut count = 0usize;
    let mut total = 0usize;
    let mut checksum = 0usize;
    for chunk in ChunkCursor::new(data, chunk_size, delimiter) {
        total += chunk.len();
        count += 1;
        for byte in chunk {
            checksum = checksum.wrapping_add(*byte as usize);
        }
    }
    black_box((count, total, checksum));
    assert_eq!(total, data.len());
}

fn consume_cursor_pattern(data: &[u8], chunk_size: usize, pattern: &[u8]) {
    let data = black_box(data);
    let mut count = 0usize;
    let mut total = 0usize;
    let mut checksum = 0usize;
    for chunk in PatternChunkCursor::new(data, chunk_size, pattern) {
        total += chunk.len();
        count += 1;
        for byte in chunk {
            checksum = checksum.wrapping_add(*byte as usize);
        }
    }
    black_box((count, total, checksum));
    assert_eq!(total, data.len());
}

fn measure_single(size: usize, tier: u8) {
    for workload in single_workloads(size, tier) {
        let expected = scan_single_memchr(&workload.data, workload.chunk_size, workload.delimiter);
        let actual = find_chunk_boundaries(&workload.data, workload.chunk_size, workload.delimiter);
        assert_eq!(
            actual, expected,
            "single-byte oracle mismatch: {}",
            workload.name
        );
        assert_valid_ranges(&workload.data, &actual);

        let iters = iterations_for(size);
        let (current, oracle) = sample_pair(
            || {
                let result = find_chunk_boundaries(
                    black_box(&workload.data),
                    black_box(workload.chunk_size),
                    black_box(workload.delimiter),
                );
                black_box(result);
            },
            || {
                let result = scan_single_memchr(
                    black_box(&workload.data),
                    black_box(workload.chunk_size),
                    black_box(workload.delimiter),
                );
                black_box(result);
            },
            iters,
        );
        let params = format!(
            "delimiter=0x{:02x} chunk_size={} candidate=current_swar",
            workload.delimiter, workload.chunk_size
        );
        print_result(
            "single/current",
            workload.name,
            size,
            &params,
            Some(size),
            iters,
            &summarize(&current),
        );
        let params = format!(
            "delimiter=0x{:02x} chunk_size={} candidate=memchr_oracle",
            workload.delimiter, workload.chunk_size
        );
        print_result(
            "single/memchr",
            workload.name,
            size,
            &params,
            Some(size),
            iters,
            &summarize(&oracle),
        );
    }
}

fn measure_pattern(size: usize, tier: u8) {
    for workload in pattern_workloads(size, tier) {
        let expected = scan_pattern_memmem(&workload.data, workload.chunk_size, workload.pattern);
        let actual =
            find_chunk_boundaries_pattern(&workload.data, workload.chunk_size, workload.pattern);
        assert_eq!(
            actual, expected,
            "pattern oracle mismatch: {}",
            workload.name
        );

        let iters = iterations_for(size);
        let (current, oracle) = sample_pair(
            || {
                let result = find_chunk_boundaries_pattern(
                    black_box(&workload.data),
                    black_box(workload.chunk_size),
                    black_box(workload.pattern),
                );
                black_box(result);
            },
            || {
                let result = scan_pattern_memmem(
                    black_box(&workload.data),
                    black_box(workload.chunk_size),
                    black_box(workload.pattern),
                );
                black_box(result);
            },
            iters,
        );
        let params = format!(
            "pattern_len={} chunk_size={} candidate=current_pattern",
            workload.pattern.len(),
            workload.chunk_size
        );
        print_result(
            "pattern/current",
            workload.name,
            size,
            &params,
            Some(size),
            iters,
            &summarize(&current),
        );
        let params = format!(
            "pattern_len={} chunk_size={} candidate=memmem_oracle",
            workload.pattern.len(),
            workload.chunk_size
        );
        print_result(
            "pattern/memmem",
            workload.name,
            size,
            &params,
            Some(size),
            iters,
            &summarize(&oracle),
        );
    }
}

fn measure_cursors(size: usize, tier: u8) {
    for workload in single_workloads(size, tier)
        .into_iter()
        .filter(|workload| matches!(workload.name, "newline_dense" | "delimiter_absent"))
    {
        let expected =
            find_chunk_boundaries(&workload.data, workload.chunk_size, workload.delimiter);
        let iters = iterations_for(size);
        let samples = sample_one(
            || consume_cursor_single(&workload.data, workload.chunk_size, workload.delimiter),
            iters,
        );
        assert_eq!(
            expected.len(),
            ChunkCursor::new(&workload.data, workload.chunk_size, workload.delimiter).count()
        );
        let params = format!(
            "delimiter=0x{:02x} chunk_size={} cursor_state_bytes={}",
            workload.delimiter,
            workload.chunk_size,
            std::mem::size_of::<ChunkCursor<'static>>()
        );
        print_result(
            "cursor/single",
            workload.name,
            size,
            &params,
            Some(size),
            iters,
            &summarize(&samples),
        );
    }

    for workload in pattern_workloads(size, tier)
        .into_iter()
        .filter(|workload| matches!(workload.name, "crlf" | "adversarial_repeated_prefix"))
    {
        let expected =
            find_chunk_boundaries_pattern(&workload.data, workload.chunk_size, workload.pattern);
        let iters = iterations_for(size);
        let samples = sample_one(
            || consume_cursor_pattern(&workload.data, workload.chunk_size, workload.pattern),
            iters,
        );
        assert_eq!(
            expected.len(),
            PatternChunkCursor::new(&workload.data, workload.chunk_size, workload.pattern).count()
        );
        let params = format!(
            "pattern_len={} chunk_size={} cursor_state_bytes={}",
            workload.pattern.len(),
            workload.chunk_size,
            std::mem::size_of::<PatternChunkCursor<'static, 'static>>()
        );
        print_result(
            "cursor/pattern",
            workload.name,
            size,
            &params,
            Some(size),
            iters,
            &summarize(&samples),
        );
    }
}

fn measure_partition(size: usize, tier: u8) {
    for workload in partition_workloads(size, tier) {
        for requested_partitions in [2usize, 8, 32] {
            let expected = find_partition_boundaries(&workload.data, requested_partitions, b'\n');
            assert_valid_ranges(&workload.data, &expected);

            let iters = iterations_for(size);
            let samples = sample_one(
                || {
                    let ranges = find_partition_boundaries(
                        black_box(&workload.data),
                        black_box(requested_partitions),
                        black_box(b'\n'),
                    );
                    black_box(ranges);
                },
                iters,
            );
            let params = format!(
                "requested_partitions={} ranges={} metadata_bytes={} range_pair_bytes={} delimiter=0x0a",
                requested_partitions,
                expected.len(),
                metadata_bytes(expected.len()),
                std::mem::size_of::<(usize, usize)>()
            );
            print_result(
                "partition",
                workload.name,
                size,
                &params,
                Some(size),
                iters,
                &summarize(&samples),
            );
        }
    }
}

fn open_mmap(path: &Path) -> MmapFile {
    // The file is immutable for the complete lifetime of this mapping.
    unsafe { MmapFile::open_path(path).expect("benchmark mmap open failed") }
}

fn warm_mapping(path: &Path) {
    let mapping = open_mmap(path);
    let mut checksum = 0u8;
    for byte in mapping.as_bytes().iter().step_by(4096) {
        checksum = checksum.wrapping_add(*byte);
    }
    black_box(checksum);
}

fn benchmark_mmap_stage(
    path: &Path,
    data: &[u8],
    size: usize,
    category: &str,
    iters: usize,
    bytes_per_iter: Option<usize>,
    operation: impl FnMut(),
) {
    let samples = sample_one(operation, iters);
    let params = "cache=WARM_CACHE cold=COLD_CACHE_NOT_RELIABLY_MEASURABLE";
    print_result(
        category,
        "jsonl_newline_dense",
        size,
        params,
        bytes_per_iter,
        iters,
        &summarize(&samples),
    );
    black_box(path);
    black_box(data);
}

fn measure_mmap(size: usize) {
    let data = repeat_to_size(b"{\"id\":1,\"status\":200,\"message\":\"ok\"}\n", size);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("benchmark-foundation");
    std::fs::create_dir_all(&root).expect("benchmark output directory creation failed");
    let path = root.join(format!("mmap-{}-{}.bin", std::process::id(), size));
    std::fs::write(&path, &data).expect("benchmark mmap fixture write failed");

    // The fixture is written once, then read/mapped and touched before samples. No cache-drop
    // privilege is used, so the result is explicitly warm-cache evidence.
    let file_copy = std::fs::read(&path).expect("benchmark mmap fixture read failed");
    black_box(file_copy);
    warm_mapping(&path);

    benchmark_mmap_stage(&path, &data, size, "mmap/open_map", 1, None, || {
        let mapping = open_mmap(&path);
        black_box(mapping.as_bytes().as_ptr());
    });

    let mapping = open_mmap(&path);
    assert_eq!(mapping.as_bytes(), data.as_slice());
    let scan_iters = iterations_for(size);
    benchmark_mmap_stage(
        &path,
        &data,
        size,
        "mmap/scan_only",
        scan_iters,
        Some(size),
        || {
            let ranges = find_chunk_boundaries(
                black_box(mapping.as_bytes()),
                black_box(chunk_size_for(size)),
                black_box(b'\n'),
            );
            black_box(ranges);
        },
    );

    benchmark_mmap_stage(
        &path,
        &data,
        size,
        "mmap/open_plus_scan",
        1,
        Some(size),
        || {
            let mapping = open_mmap(&path);
            let ranges = find_chunk_boundaries(
                black_box(mapping.as_bytes()),
                black_box(chunk_size_for(size)),
                black_box(b'\n'),
            );
            black_box(ranges);
        },
    );

    benchmark_mmap_stage(
        &path,
        &data,
        size,
        "mmap/open_scan_byte_sum",
        1,
        Some(size),
        || {
            let mapping = open_mmap(&path);
            let ranges = find_chunk_boundaries(
                black_box(mapping.as_bytes()),
                black_box(chunk_size_for(size)),
                black_box(b'\n'),
            );
            let mut checksum = 0usize;
            for byte in mapping.as_bytes() {
                checksum = checksum.wrapping_add(*byte as usize);
            }
            black_box((ranges, checksum));
        },
    );

    drop(mapping);
    let _ = std::fs::remove_file(&path);
}

fn timer_floor_ns() -> u128 {
    let mut floor = u128::MAX;
    for _ in 0..1_000 {
        let start = Instant::now();
        let elapsed = start.elapsed().as_nanos();
        if elapsed > 0 {
            floor = floor.min(elapsed);
        }
    }
    if floor == u128::MAX {
        0
    } else {
        floor
    }
}

fn tier_from_environment() -> u8 {
    match std::env::var("MMAP_BENCH_TIER")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
    {
        Some(2) => 2,
        Some(3) => 3,
        _ => 1,
    }
}

fn sizes_for_tier(tier: u8) -> &'static [usize] {
    match tier {
        2 => TIER_2_SIZES,
        3 => TIER_3_SIZES,
        _ => TIER_1_SIZES,
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[test]
#[ignore = "benchmark foundation — run with --release --ignored --nocapture"]
#[allow(clippy::assertions_on_constants)]
fn benchmark_foundation() {
    assert!(
        !cfg!(debug_assertions),
        "benchmark_foundation requires a release build"
    );

    let tier = tier_from_environment();
    println!("=== mmap-chunker-core benchmark foundation ===");
    println!(
        "tier={} os={} arch={} build=release rustc={} logical_cpus={}",
        tier,
        std::env::consts::OS,
        std::env::consts::ARCH,
        rustc_version(),
        std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1)
    );
    println!(
        "contract=samples:{} warmups:{} p10/p50/p90/p95/min/max/mean/relative_spread/bytes/throughput",
        SAMPLE_COUNT, WARMUP_COUNT
    );
    println!(
        "measurement=per_iter_normalized_once; pair_order=alternating; timer_floor={}ns",
        timer_floor_ns()
    );
    println!(
        "bytes_semantics=logical_input_span_per_iteration; boundary_planners_may_examine_less"
    );
    println!(
        "metadata=range_pair:{}B cursor_state:{}B pattern_cursor_state:{}B",
        std::mem::size_of::<(usize, usize)>(),
        std::mem::size_of::<ChunkCursor<'static>>(),
        std::mem::size_of::<PatternChunkCursor<'static, 'static>>()
    );

    for &size in sizes_for_tier(tier) {
        println!("--- size={}B ---", size);
        measure_single(size, tier);
        measure_pattern(size, tier);
        measure_cursors(size, tier);
        measure_partition(size, tier);
        measure_mmap(size);
    }
    println!("=== benchmark foundation complete ===");
}

#[test]
fn throughput_contract_is_batch_invariant() {
    let bytes_per_iter = 1_048_576.0;
    let elapsed_per_iter_seconds = 0.002;
    let iters = 17.0;
    let per_iter = throughput_bytes_per_second(bytes_per_iter, elapsed_per_iter_seconds).unwrap();
    let batched =
        throughput_bytes_per_second(bytes_per_iter * iters, elapsed_per_iter_seconds * iters)
            .unwrap();

    assert!((per_iter - 524_288_000.0).abs() < 0.1);
    assert!((batched - per_iter).abs() < 0.1);
}
