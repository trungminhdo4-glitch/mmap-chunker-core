//! Reproducible, manual performance baseline for the current engine.
//!
//! This is deliberately std-only: it measures the public Rust API on
//! deterministic, temporary fixtures without adding a benchmark framework to
//! the library. Run one of:
//!
//! ```text
//! MMAP_BENCH_TIER=smoke    cargo test --release --test performance_baseline -- --ignored --nocapture
//! MMAP_BENCH_TIER=standard cargo test --release --test performance_baseline -- --ignored --nocapture
//! MMAP_BENCH_TIER=large    cargo test --release --test performance_baseline -- --ignored --nocapture
//! ```
//!
//! `smoke` is a 10 MiB CI/local sanity check. `standard` covers 10 and 100
//! MiB. `large` runs only the opt-in 1 GiB representative subset.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mmap_chunker_core::{MmapChunker, PatternChunkCursor};

const MIB: usize = 1024 * 1024;
const CHUNK_SIZE: usize = MIB;
const REPEAT: usize = 7;

type Measurements = (Vec<Duration>, usize, usize);

#[derive(Clone, Copy)]
enum Layout {
    Byte(u8),
    Pattern(&'static [u8]),
}

#[derive(Clone, Copy)]
struct Fixture {
    name: &'static str,
    layout: Layout,
    final_delimiter: bool,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "fixed_96b_lf",
        layout: Layout::Byte(b'\n'),
        final_delimiter: true,
    },
    Fixture {
        name: "jsonl_uneven_lf",
        layout: Layout::Byte(b'\n'),
        final_delimiter: true,
    },
    Fixture {
        name: "long_records_lf",
        layout: Layout::Byte(b'\n'),
        final_delimiter: false,
    },
    Fixture {
        name: "delimiter_dense_lf",
        layout: Layout::Byte(b'\n'),
        final_delimiter: true,
    },
    Fixture {
        name: "delimiter_sparse_lf",
        layout: Layout::Byte(b'\n'),
        final_delimiter: false,
    },
    Fixture {
        name: "crlf",
        layout: Layout::Pattern(b"\r\n"),
        final_delimiter: true,
    },
    Fixture {
        name: "custom_end_record",
        layout: Layout::Pattern(b"END_RECORD\x00"),
        final_delimiter: false,
    },
    Fixture {
        name: "custom_pattern_absent",
        layout: Layout::Pattern(b"END_RECORD\x00"),
        final_delimiter: false,
    },
];

fn tier_sizes() -> Vec<usize> {
    match std::env::var("MMAP_BENCH_TIER").as_deref() {
        Ok("large") => vec![1024 * MIB],
        Ok("standard") => vec![10 * MIB, 100 * MIB],
        Ok("smoke") | Err(_) => vec![10 * MIB],
        Ok(other) => panic!("MMAP_BENCH_TIER must be smoke, standard, or large; got {other}"),
    }
}

fn write_fixture(path: &Path, fixture: Fixture, target: usize) {
    let mut data = Vec::with_capacity(target + 256 * 1024);
    let delimiter = match fixture.layout {
        Layout::Byte(b) => vec![b],
        Layout::Pattern(p) => p.to_vec(),
    };
    let payload = match fixture.name {
        "fixed_96b_lf" => b"2026-08-14T00:00:00Z,service=api,request=00000000,status=200,latency_us=42,payload=abcdefgh\n".as_slice(),
        "jsonl_uneven_lf" => b"{\"ts\":\"2026-08-14T00:00:00Z\",\"user\":1234,\"ok\":true,\"payload\":\"variable-width-record\"}\n".as_slice(),
        "long_records_lf" => b"L".as_slice(),
        "delimiter_dense_lf" => b"x\n".as_slice(),
        "delimiter_sparse_lf" => b"S".as_slice(),
        "crlf" => b"method=GET path=/v1/items status=200 bytes=1234\r\n".as_slice(),
        "custom_end_record" => b"field1=alpha;field2=bravo;field3=charlie;END_RECORD\x00".as_slice(),
        "custom_pattern_absent" => b"X".as_slice(),
        _ => unreachable!(),
    };

    let mut record = 0usize;
    while data.len() < target {
        match fixture.name {
            "jsonl_uneven_lf" => {
                data.extend_from_slice(&payload[..payload.len() - 1]);
                data.extend(std::iter::repeat(b'a').take((record.wrapping_mul(97) % 733) + 1));
                data.extend_from_slice(&delimiter);
            }
            "long_records_lf" => {
                // Larger than the 1 MiB requested chunk: exercises boundary
                // search and collapsed record-aligned partitions.
                data.extend(std::iter::repeat(b'L').take(4 * MIB));
                data.extend_from_slice(&delimiter);
            }
            "delimiter_sparse_lf" => {
                // Delimiters are deliberately much farther apart than the
                // requested chunk, so each boundary search traverses MiBs.
                data.extend(std::iter::repeat(b'S').take(8 * MIB));
                data.extend_from_slice(&delimiter);
            }
            _ => data.extend_from_slice(payload),
        }
        record += 1;
    }
    data.truncate(target);
    if fixture.final_delimiter && data.len() >= delimiter.len() {
        let start = data.len() - delimiter.len();
        data[start..].copy_from_slice(&delimiter);
    }
    if !fixture.final_delimiter && data.ends_with(&delimiter) {
        let last = data.len() - 1;
        data[last] = b'X';
    }
    std::fs::write(path, data).unwrap();
}

fn median_ns(samples: &[Duration]) -> u128 {
    let mut ns: Vec<u128> = samples.iter().map(Duration::as_nanos).collect();
    ns.sort_unstable();
    ns[ns.len() / 2]
}

fn describe(label: &str, touched_bytes: Option<usize>, samples: &[Duration]) {
    let median = median_ns(samples);
    let min = samples.iter().map(Duration::as_nanos).min().unwrap();
    let max = samples.iter().map(Duration::as_nanos).max().unwrap();
    match touched_bytes {
        Some(bytes) => {
            let gib_s = bytes as f64 / median as f64 * 1_000_000_000.0 / 1024_f64.powi(3);
            println!("RESULT op={label} median_ms={:.3} min_ms={:.3} max_ms={:.3} touched_mib={:.3} effective_gib_s={gib_s:.2} reps={}", median as f64 / 1e6, min as f64 / 1e6, max as f64 / 1e6, bytes as f64 / MIB as f64, samples.len());
        }
        None => println!("RESULT op={label} median_ms={:.3} min_ms={:.3} max_ms={:.3} touched_mib=not_applicable reps={}", median as f64 / 1e6, min as f64 / 1e6, max as f64 / 1e6, samples.len()),
    }
}

fn chunk_search_bytes(ranges: &[(usize, usize)], len: usize) -> usize {
    ranges
        .iter()
        .map(|&(start, end)| {
            let target = start.saturating_add(CHUNK_SIZE);
            if target < len {
                end.saturating_sub(target)
            } else {
                0
            }
        })
        .sum()
}

fn assert_cover(
    data: &[u8],
    ranges: &[(usize, usize)],
    delimiter: &[u8],
    require_record_boundaries: bool,
) {
    assert!(!ranges.is_empty());
    let mut position = 0;
    for (index, &(start, end)) in ranges.iter().enumerate() {
        assert_eq!(start, position, "gap/overlap at {index}");
        assert!(end > start && end <= data.len());
        if require_record_boundaries && end < data.len() {
            assert!(data[..end].ends_with(delimiter), "record split at {index}");
        }
        position = end;
    }
    assert_eq!(position, data.len());
}

fn measure_open(path: &Path) -> Vec<Duration> {
    (0..REPEAT)
        .map(|_| {
            let started = Instant::now();
            let file = unsafe { MmapChunker::open(path).unwrap() };
            black_box(file.len());
            started.elapsed()
        })
        .collect()
}

fn measure_byte_eager(path: &Path, delimiter: u8) -> (Vec<Duration>, usize, usize) {
    let mut count = 0;
    let mut scanned = 0;
    let samples = (0..REPEAT)
        .map(|_| {
            let mut file = unsafe { MmapChunker::open(path).unwrap() };
            let started = Instant::now();
            count = file.scan_delimited(CHUNK_SIZE, delimiter);
            let ranges = (0..count)
                .map(|i| {
                    let c = file.get_chunk(i).unwrap();
                    let s = c.as_ptr() as usize - file.as_bytes().as_ptr() as usize;
                    (s, s + c.len())
                })
                .collect::<Vec<_>>();
            scanned = chunk_search_bytes(&ranges, file.len());
            assert_cover(file.as_bytes(), &ranges, &[delimiter], true);
            started.elapsed()
        })
        .collect();
    (samples, count, scanned)
}

fn measure_byte_lazy(path: &Path, delimiter: u8) -> (Vec<Duration>, usize, usize) {
    let mut count = 0;
    let mut scanned = 0;
    let samples = (0..REPEAT)
        .map(|_| {
            let file = unsafe { MmapChunker::open(path).unwrap() };
            let started = Instant::now();
            let mut ranges = Vec::new();
            for chunk in file.delimited_cursor(CHUNK_SIZE, delimiter) {
                let start = chunk.as_ptr() as usize - file.as_bytes().as_ptr() as usize;
                ranges.push((start, start + chunk.len()));
            }
            count = ranges.len();
            scanned = chunk_search_bytes(&ranges, file.len());
            assert_cover(file.as_bytes(), &ranges, &[delimiter], true);
            started.elapsed()
        })
        .collect();
    (samples, count, scanned)
}

fn measure_pattern(path: &Path, pattern: &'static [u8]) -> (Measurements, Measurements) {
    let mut eager_count = 0;
    let mut eager_scanned = 0;
    let eager = (0..REPEAT)
        .map(|_| {
            let mut file = unsafe { MmapChunker::open(path).unwrap() };
            let started = Instant::now();
            eager_count = file.scan_delimited_pattern(CHUNK_SIZE, pattern);
            let ranges: Vec<_> = (0..eager_count)
                .map(|i| {
                    let c = file.get_chunk(i).unwrap();
                    let s = c.as_ptr() as usize - file.as_bytes().as_ptr() as usize;
                    (s, s + c.len())
                })
                .collect();
            eager_scanned = chunk_search_bytes(&ranges, file.len());
            assert_cover(file.as_bytes(), &ranges, pattern, true);
            started.elapsed()
        })
        .collect();
    let mut lazy_count = 0;
    let mut lazy_scanned = 0;
    let lazy = (0..REPEAT)
        .map(|_| {
            let file = unsafe { MmapChunker::open(path).unwrap() };
            let started = Instant::now();
            let mut ranges = Vec::new();
            for chunk in PatternChunkCursor::new(file.as_bytes(), CHUNK_SIZE, pattern) {
                let start = chunk.as_ptr() as usize - file.as_bytes().as_ptr() as usize;
                ranges.push((start, start + chunk.len()));
            }
            lazy_count = ranges.len();
            lazy_scanned = chunk_search_bytes(&ranges, file.len());
            assert_cover(file.as_bytes(), &ranges, pattern, true);
            started.elapsed()
        })
        .collect();
    (
        (eager, eager_count, eager_scanned),
        (lazy, lazy_count, lazy_scanned),
    )
}

fn scalar_count(data: &[u8], delimiter: u8) -> usize {
    data.iter().filter(|&&b| b == delimiter).count()
}

fn measure_byte_workloads(path: &Path, delimiter: u8) {
    let len = std::fs::metadata(path).unwrap().len() as usize;
    describe("open_map_only", None, &measure_open(path));
    let (eager, eager_count, eager_scanned) = measure_byte_eager(path, delimiter);
    describe("single_byte_eager_indexed", Some(eager_scanned), &eager);
    let (lazy, lazy_count, lazy_scanned) = measure_byte_lazy(path, delimiter);
    describe("single_byte_lazy_full_consume", Some(lazy_scanned), &lazy);
    assert_eq!(eager_count, lazy_count);
    println!(
        "META chunks={eager_count} indexed_bytes_estimate={} cursor_state_bytes={}",
        eager_count * std::mem::size_of::<(usize, usize)>(),
        std::mem::size_of::<mmap_chunker_core::ChunkCursor<'_>>()
    );

    let readback: Vec<_> = (0..REPEAT)
        .map(|_| {
            let mut file = unsafe { MmapChunker::open(path).unwrap() };
            let count = file.scan_delimited(CHUNK_SIZE, delimiter);
            let started = Instant::now();
            let total: usize = (0..count).map(|i| file.get_chunk(i).unwrap().len()).sum();
            assert_eq!(total, len);
            started.elapsed()
        })
        .collect();
    describe("indexed_chunk_retrieval_after_scan", None, &readback);

    let scalar: Vec<_> = (0..REPEAT)
        .map(|_| {
            let file = unsafe { MmapChunker::open(path).unwrap() };
            let started = Instant::now();
            black_box(scalar_count(file.as_bytes(), delimiter));
            started.elapsed()
        })
        .collect();
    describe(
        "scalar_full_file_delimiter_count_reference",
        Some(len),
        &scalar,
    );

    let fixed: Vec<_> = (0..REPEAT)
        .map(|_| {
            let mut file = unsafe { MmapChunker::open(path).unwrap() };
            let started = Instant::now();
            let count = file.scan_fixed(CHUNK_SIZE);
            let total: usize = (0..count).map(|i| file.get_chunk(i).unwrap().len()).sum();
            assert_eq!(total, len);
            started.elapsed()
        })
        .collect();
    describe("fixed_size_plan_and_retrieve", None, &fixed);

    for &n in &[1, 2, 4, 8, 16, 32] {
        let partition: Vec<_> = (0..REPEAT)
            .map(|_| {
                let mut file = unsafe { MmapChunker::open(path).unwrap() };
                let started = Instant::now();
                let count = file.partition_records(n, delimiter);
                let ranges: Vec<_> = (0..count)
                    .map(|i| {
                        let c = file.get_chunk(i).unwrap();
                        let s = c.as_ptr() as usize - file.as_bytes().as_ptr() as usize;
                        (s, s + c.len())
                    })
                    .collect();
                assert_cover(file.as_bytes(), &ranges, &[delimiter], true);
                started.elapsed()
            })
            .collect();
        describe(&format!("partition_records_n{n}"), None, &partition);
    }
}

#[test]
#[ignore = "manual performance baseline; see module documentation"]
#[allow(clippy::assertions_on_constants)]
fn performance_baseline() {
    assert!(!cfg!(debug_assertions), "release mode is required");
    let tier = std::env::var("MMAP_BENCH_TIER").unwrap_or_else(|_| "smoke".into());
    // Keep generated fixtures inside Cargo's ignored build directory. This
    // avoids writing outside the active project and makes interrupted runs
    // easy to identify and remove.
    let root = PathBuf::from("target")
        .join("perf-fixtures")
        .join(format!("run_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    println!(
        "BASELINE tier={tier} os={} arch={} chunk_size={} repeats={REPEAT}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        CHUNK_SIZE
    );
    for size in tier_sizes() {
        for fixture in FIXTURES {
            if size == 1024 * MIB
                && !matches!(fixture.name, "jsonl_uneven_lf" | "delimiter_sparse_lf")
            {
                continue;
            }
            let path: PathBuf = root.join(format!("{}_{}.dat", fixture.name, size / MIB));
            write_fixture(&path, *fixture, size);
            println!(
                "FIXTURE name={} size_mib={} final_delimiter={}",
                fixture.name,
                size / MIB,
                fixture.final_delimiter
            );
            match fixture.layout {
                Layout::Byte(delimiter) => measure_byte_workloads(&path, delimiter),
                Layout::Pattern(pattern) => {
                    let ((eager, eager_count, eager_scanned), (lazy, lazy_count, lazy_scanned)) =
                        measure_pattern(&path, pattern);
                    describe("multi_byte_eager_indexed", Some(eager_scanned), &eager);
                    describe("multi_byte_lazy_full_consume", Some(lazy_scanned), &lazy);
                    assert_eq!(eager_count, lazy_count);
                    println!("META chunks={eager_count} indexed_bytes_estimate={} pattern_cursor_state_bytes={}", eager_count * std::mem::size_of::<(usize, usize)>(), std::mem::size_of::<PatternChunkCursor<'_, '_>>());
                }
            }
            std::fs::remove_file(&path).unwrap();
        }
    }
    std::fs::remove_dir_all(&root).unwrap();
}
