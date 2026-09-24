//! Performance baseline: mmap chunker vs std::fs::read.
//!
//! Run manually:  cargo test --test benchmark -- --nocapture --ignored
//!
//! Creates temp files, runs multiple iterations, reports wall-clock and
//! throughput.  No external dependencies — std only.

use std::ffi::CString;
use std::time::Instant;

use mmap_chunker_core::{
    plan_partition_ranges, MmapFile, PlannerOptions, SourceMode, DEFAULT_SCAN_BUFFER_BYTES,
    MIN_WINDOW_BYTES,
};

use mmap_chunker_core::{CChunkView, CEngineHandle};

extern "C" {
    fn mmap_engine_open(path: *const std::ffi::c_char) -> *mut CEngineHandle;
    fn mmap_engine_scan_chunks(handle: *mut CEngineHandle, chunk_size_bytes: usize) -> usize;
    #[allow(dead_code)]
    fn mmap_engine_get_chunk(handle: *mut CEngineHandle, index: usize, out: *mut CChunkView)
        -> i32;
    fn mmap_engine_free(handle: *mut CEngineHandle);
}

const TEST_LINE: &[u8] = b"2024-01-15T10:30:00Z,event_type_alpha,192.168.1.100,user_12345,session_abc,payload_00042,status_ok\n";
const WARMUP_ITERS: u32 = 2;
const BENCH_ITERS: u32 = 5;

fn create_temp_file(size_mb: usize) -> (std::path::PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join("mmap_bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("bench_{}mb.dat", size_mb));

    let line_len = TEST_LINE.len();
    let target = size_mb * 1024 * 1024;
    let total_lines = target / line_len;
    let content = TEST_LINE.repeat(total_lines);
    std::fs::write(&path, &content).unwrap();
    (path, content)
}

fn run_mmap(path: &std::path::Path, chunk_size: usize) -> (u64, usize) {
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    let start = Instant::now();
    unsafe {
        let h = mmap_engine_open(c_path.as_ptr());
        assert!(!h.is_null());
        let count = mmap_engine_scan_chunks(h, chunk_size);
        let elapsed = start.elapsed().as_micros() as u64;
        mmap_engine_free(h);
        (elapsed, count)
    }
}

fn run_fs_read(path: &std::path::Path, chunk_size: usize) -> (u64, usize) {
    let start = Instant::now();
    let data = std::fs::read(path).unwrap();
    let read_us = start.elapsed().as_micros() as u64;

    let start_scan = Instant::now();
    let chunks = mmap_chunker_core::scanner::find_chunk_boundaries(&data, chunk_size, b'\n');
    let scan_us = start_scan.elapsed().as_micros() as u64;

    (read_us + scan_us, chunks.len())
}

#[test]
#[ignore = "benchmark — run manually with --ignored --nocapture"]
fn benchmark() {
    println!();
    println!("=== mmap-chunker-core Performance Baseline ===");
    println!("System: {}", std::env::consts::OS);
    println!();

    let file_sizes_mb: [usize; 3] = [1, 16, 64];
    let chunk_sizes_kb: [usize; 3] = [64, 256, 1024];

    println!(
        "{0:>8} {1:>8} {2:>12} {3:>12} {4:>12} {5:>12} {6:>10}",
        "File MB", "Chunk KB", "mmap us", "fs_read us", "mmap MB/s", "fs MB/s", "Chunks"
    );
    println!(
        "{:-<8} {:-<8} {:-<12} {:-<12} {:-<12} {:-<12} {:-<10}",
        "", "", "", "", "", "", ""
    );

    for file_mb in &file_sizes_mb {
        let (path, _content) = create_temp_file(*file_mb);
        let file_bytes = (*file_mb * 1024 * 1024) as f64;

        for chunk_kb in &chunk_sizes_kb {
            let chunk_size = chunk_kb * 1024;

            // Warm up
            for _ in 0..WARMUP_ITERS {
                let _ = run_mmap(&path, chunk_size);
                let _ = run_fs_read(&path, chunk_size);
            }

            // Benchmark
            let mut mmap_total = 0u64;
            let mut fs_total = 0u64;
            let mut last_chunks = 0usize;

            for _ in 0..BENCH_ITERS {
                let (us, chunks) = run_mmap(&path, chunk_size);
                mmap_total += us;
                last_chunks = chunks;

                let (us, _chunks) = run_fs_read(&path, chunk_size);
                fs_total += us;
            }

            let mmap_avg = mmap_total as f64 / BENCH_ITERS as f64;
            let fs_avg = fs_total as f64 / BENCH_ITERS as f64;
            let mmap_mbps = if mmap_avg > 0.0 {
                file_bytes / mmap_avg * 1e6 / (1024.0 * 1024.0)
            } else {
                0.0
            };
            let fs_mbps = if fs_avg > 0.0 {
                file_bytes / fs_avg * 1e6 / (1024.0 * 1024.0)
            } else {
                0.0
            };

            println!("{file_mb:>8} {chunk_kb:>8} {mmap_avg:>12.0} {fs_avg:>12.0} {mmap_mbps:>12.1} {fs_mbps:>12.1} {last_chunks:>10}",
                file_mb = file_mb,
                chunk_kb = chunk_kb,
                mmap_avg = mmap_avg,
                fs_avg = fs_avg,
                mmap_mbps = mmap_mbps,
                fs_mbps = fs_mbps,
                last_chunks = last_chunks,
            );
        }

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("mmap_bench"));
    }

    println!();
    println!("Warmup iterations: {WARMUP_ITERS}, Bench iterations: {BENCH_ITERS}");
    println!("Note: Results include page cache effects. Warm runs may be faster than cold.");
    println!("Note: mmap column includes open+scan time. fs_read includes read+scan.");
}

fn create_crlf_temp_file(size_mb: usize) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("mmap_bench_sources");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("bench_sources_{}mb.log", size_mb));

    let line = b"2024-01-15T10:30:00Z,event_type_alpha,192.168.1.100,user_12345\r\n";
    let target = size_mb * 1024 * 1024;
    let total_lines = target / line.len();
    std::fs::write(&path, line.repeat(total_lines)).unwrap();
    path
}

/// Compare the range planner backends on the same file and parameters.
///
/// Run manually: `cargo test --release --test benchmark benchmark_source_modes -- --ignored --nocapture`
#[test]
#[ignore = "benchmark — run manually with --ignored --nocapture"]
fn benchmark_source_modes() {
    println!();
    println!("=== Range planner source modes (CRLF, 16 partitions, 64 MiB window) ===");
    println!("System: {}", std::env::consts::OS);
    println!();
    println!(
        "{0:>28} {1:>12} {2:>12} {3:>10}",
        "backend", "avg us", "MB/s", "ranges"
    );
    println!("{:-<28} {:-<12} {:-<12} {:-<10}", "", "", "", "");

    let file_mb = 64usize;
    let path = create_crlf_temp_file(file_mb);
    let file_bytes = (file_mb * 1024 * 1024) as f64;
    let partitions = 16usize;

    // Baseline: full mmap + slice scanner (no planner abstraction).
    {
        let mut total = 0u64;
        let mut last = 0usize;
        for _ in 0..WARMUP_ITERS {
            let _ = baseline(&path, partitions);
        }
        for _ in 0..BENCH_ITERS {
            let (us, ranges) = baseline(&path, partitions);
            total += us;
            last = ranges;
        }
        let avg = total as f64 / BENCH_ITERS as f64;
        println!(
            "{:>28} {:>12.0} {:>12.1} {:>10}",
            "mmap + slice scanner",
            avg,
            file_bytes / avg * 1e6 / (1024.0 * 1024.0),
            last
        );
    }

    let modes = [
        (SourceMode::Mmap, 0usize, "planner: mmap"),
        (
            SourceMode::Windowed,
            64 * 1024 * 1024,
            "planner: windowed 64M",
        ),
        (SourceMode::Pread, 0, "planner: pread"),
    ];

    for (mode, window, label) in modes {
        let options = PlannerOptions::new(mode)
            .with_window_bytes(window.max(MIN_WINDOW_BYTES))
            .with_scan_buffer_bytes(DEFAULT_SCAN_BUFFER_BYTES);

        for _ in 0..WARMUP_ITERS {
            let _ = unsafe { plan_partition_ranges(&path, partitions, b"\r\n", &options) };
        }

        let mut total = 0u64;
        let mut last = 0usize;
        for _ in 0..BENCH_ITERS {
            let start = Instant::now();
            let ranges =
                unsafe { plan_partition_ranges(&path, partitions, b"\r\n", &options) }.unwrap();
            total += start.elapsed().as_micros() as u64;
            last = ranges.len();
        }
        let avg = total as f64 / BENCH_ITERS as f64;
        println!(
            "{:>28} {:>12.0} {:>12.1} {:>10}",
            label,
            avg,
            file_bytes / avg * 1e6 / (1024.0 * 1024.0),
            last
        );
    }

    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("mmap_bench_sources"));
    println!();
    println!("Warmup iterations: {WARMUP_ITERS}, Bench iterations: {BENCH_ITERS}");
}

fn baseline(path: &std::path::Path, partitions: usize) -> (u64, usize) {
    let start = Instant::now();
    let mmap = unsafe { MmapFile::open_path(path).unwrap() };
    let ranges = mmap_chunker_core::scanner::find_partition_boundaries_pattern(
        mmap.as_bytes(),
        partitions,
        b"\r\n",
    );
    (start.elapsed().as_micros() as u64, ranges.len())
}

/// Baseline probe for a future single-lap fusion of the three CLI laps
/// (`partition` ~ `plan` ~ `index`).
///
/// Runs the lap equivalents behind the CLI on one small deterministic
/// fixture (1 MiB, in-temp) and quantifies the replayed work a fused
/// single pass (S6+H2+S2 idea) could eliminate:
///
/// * `partition` lap — [`plan_partition_ranges_with`]
/// * `plan` lap — [`plan_file_with_framing`] (re-scans; must be identical)
/// * `index` lap — [`build_record_index`] +
///   [`plan_partition_boundaries_from_index`]
///
/// `pread` syscalls are counted with a test-local [`ByteSource`]
/// instrumentor that forces the buffered path (`as_slice() == None`), plus
/// analytic post-target search bytes and boundary replays. Read-only probe:
/// asserts invariants, changes no behavior, touches no `src/` code.
///
/// Run manually:
/// `cargo test --test benchmark fused_lap_saving_probe -- --ignored --nocapture`
#[test]
#[ignore = "benchmark — run manually with --ignored --nocapture"]
fn fused_lap_saving_probe() {
    use mmap_chunker_core::{
        build_record_index, plan_file_with_framing, plan_partition_boundaries,
        plan_partition_boundaries_from_index, plan_partition_ranges_with, BuiltinFraming,
        ByteSource, FramingStrategy, DEFAULT_WINDOW_BYTES,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROBE_TARGET_BYTES: usize = 1024 * 1024;
    const PROBE_MAX_BYTES: usize = 2 * 1024 * 1024;
    const PROBE_PARTS: usize = 8;
    const PROBE_STRIDE: u64 = 64;
    const PROBE_SCAN_BUF: usize = 4096;
    const PROBE_RECORD_BYTES: usize = 182;

    /// [`ByteSource`] without a contiguous slice (like `PreadSource`) that
    /// counts every positional read: one `read_at` == one logical `pread`.
    /// Method limits: this counts logical reads, not syscalls or physical
    /// I/O (short-read loops, page cache, and mmap's zero-`read_at`
    /// fast path are not modeled; see `as_slice` in `src/source.rs`).
    struct CountingSource {
        data: Vec<u8>,
        calls: AtomicUsize,
        bytes: AtomicUsize,
    }

    impl CountingSource {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                calls: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
            }
        }

        fn counters(&self) -> (usize, usize) {
            (
                self.calls.load(Ordering::Relaxed),
                self.bytes.load(Ordering::Relaxed),
            )
        }
    }

    impl ByteSource for CountingSource {
        fn len(&self) -> usize {
            self.data.len()
        }

        fn read_at(&self, offset: usize, out: &mut [u8]) -> std::io::Result<usize> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if out.is_empty() || offset >= self.data.len() {
                return Ok(0);
            }
            let n = out.len().min(self.data.len() - offset);
            out[..n].copy_from_slice(&self.data[offset..offset + n]);
            self.bytes.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }

        fn as_slice(&self) -> Option<&[u8]> {
            None
        }
    }

    // Deterministic fixed-length LF records (182 B each).
    let filler = "0123456789abcdef".repeat(10);
    let mut content = Vec::with_capacity(PROBE_TARGET_BYTES + 256);
    let mut seq: u32 = 0;
    while content.len() < PROBE_TARGET_BYTES {
        let line = format!("rec-{seq:08}-payload-{filler}");
        content.extend_from_slice(line.as_bytes());
        content.push(b'\n');
        seq += 1;
    }
    let file_len = content.len();
    assert!(
        file_len <= PROBE_MAX_BYTES,
        "probe fixture must stay small, got {file_len} bytes"
    );
    let record_count = content.iter().filter(|&&b| b == b'\n').count() as u64;
    assert_eq!(
        file_len,
        record_count as usize * PROBE_RECORD_BYTES,
        "fixture must be fixed-length records"
    );

    let dir = std::env::temp_dir().join("mmap_fused_probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("probe.log");
    std::fs::write(&path, &content).unwrap();

    let strategy = BuiltinFraming::delimiter(vec![b'\n']).unwrap();
    let mmap_options = PlannerOptions::new(SourceMode::Mmap);

    // Lap 1 (`partition` CLI): raw ranges.
    let ranges_partition =
        unsafe { plan_partition_ranges_with(&path, PROBE_PARTS, &strategy, &mmap_options) }
            .unwrap();
    // Lap 2 (`plan` CLI): sealed manifest; re-scans the file.
    let plan = unsafe {
        plan_file_with_framing(
            &path,
            PROBE_PARTS,
            &strategy,
            SourceMode::Mmap,
            DEFAULT_WINDOW_BYTES,
        )
    }
    .unwrap();
    // Lap 3 (`index` CLI): sparse sidecar + record-count-balanced ranges.
    let index =
        unsafe { build_record_index(&path, &strategy, PROBE_STRIDE, &mmap_options) }.unwrap();
    let ranges_index = plan_partition_boundaries_from_index(&index, PROBE_PARTS, file_len).unwrap();

    // partition vs plan: same scan, byte-identical ranges (lap 2 is a replay).
    assert_eq!(
        ranges_partition, plan.ranges,
        "plan lap must replay partition ranges exactly"
    );
    assert_eq!(
        ranges_partition.len(),
        PROBE_PARTS,
        "uniform small records must yield all requested partitions"
    );
    let mut cursor = 0usize;
    for (start, end) in &ranges_partition {
        assert_eq!(*start, cursor, "partition ranges must be contiguous");
        assert!(*end > *start, "partition ranges must be non-empty");
        cursor = *end;
    }
    assert_eq!(cursor, file_len, "partition ranges must cover the file");

    // Index lap: same record count, ranges cover the file on record boundaries.
    assert_eq!(
        index.record_count, record_count,
        "index must see every record"
    );
    assert_eq!(index.stride, PROBE_STRIDE);
    let mut cursor = 0usize;
    for (start, end) in &ranges_index {
        assert_eq!(*start, cursor, "index ranges must be contiguous");
        assert!(*end > *start, "index ranges must be non-empty");
        cursor = *end;
    }
    assert_eq!(cursor, file_len, "index ranges must cover the file");
    assert!(
        ranges_index.len() <= PROBE_PARTS,
        "index ranges must not exceed the requested partitions"
    );
    for (_, end) in ranges_index
        .iter()
        .take(ranges_index.len().saturating_sub(1))
    {
        assert_eq!(
            content[*end - 1],
            b'\n',
            "non-final index range must end on a record boundary"
        );
    }

    // Analytic second-pass cost: bytes scanned past each ideal target.
    // The planner places each cut at the smallest boundary strictly after
    // `target`, so every term is positive by construction.
    let n = ranges_partition.len();
    let mut post_target_sum = 0usize;
    for (i, (start, _)) in ranges_partition.iter().enumerate().skip(1) {
        let target = ((file_len as u128) * (i as u128) / (n as u128)) as usize;
        assert!(
            *start > target,
            "cut {i} must lie strictly after its target {target}"
        );
        post_target_sum += start - target;
    }
    let internal_cuts = n - 1;
    assert!(
        post_target_sum > 0,
        "forward search past targets must cost bytes"
    );
    // Plan lap re-derives every partition cut: pure boundary replay.
    let replayed_cuts = internal_cuts;
    assert!(
        replayed_cuts > 0,
        "a second lap must replay cut derivations"
    );

    // Instrumented `pread` counting on the buffered (non-slice) path.
    let counting_partition = CountingSource::new(&content);
    let instrumented =
        plan_partition_boundaries(&counting_partition, PROBE_PARTS, b"\n", PROBE_SCAN_BUF).unwrap();
    assert_eq!(
        instrumented, ranges_partition,
        "buffered backend must be range-equivalent (project invariant)"
    );
    let (calls_partition, bytes_partition) = counting_partition.counters();
    assert!(
        calls_partition > 0,
        "partition lap must issue positional reads"
    );

    // Plan-lap replay through the same buffered path: must cost the same.
    let counting_replay = CountingSource::new(&content);
    let replayed =
        plan_partition_boundaries(&counting_replay, PROBE_PARTS, b"\n", PROBE_SCAN_BUF).unwrap();
    assert_eq!(replayed, ranges_partition);
    let (calls_replay, bytes_replay) = counting_replay.counters();
    assert_eq!(
        (calls_replay, bytes_replay),
        (calls_partition, bytes_partition),
        "lap replay must cost exactly the same syscalls/bytes (deterministic)"
    );

    // Index-lap full walk: visit every record end like `scan_record_starts`.
    let counting_index = CountingSource::new(&content);
    let mut walk_buf = vec![0u8; PROBE_SCAN_BUF];
    let mut walker = strategy.scanner(&counting_index, &mut walk_buf);
    let mut walk_cursor = 0usize;
    let mut walk_records: u64 = 0;
    loop {
        match walker.boundary_after(walk_cursor).unwrap() {
            Some(end) => {
                walk_records += 1;
                if end >= file_len {
                    break;
                }
                walk_cursor = end;
            }
            None => {
                walk_records += 1;
                break;
            }
        }
    }
    assert_eq!(
        walk_records, record_count,
        "instrumented walk must visit every record"
    );
    let (calls_index, bytes_index) = counting_index.counters();
    assert!(calls_index > 0, "index walk must issue positional reads");

    // Fusion arithmetic: one pass that builds the index already visits every
    // record end, so partition cuts come from the same pass for free. The
    // fused cost is bounded by the index walk; the two partition-lap replays
    // are the provable saving.
    let separate_calls = calls_partition + calls_replay + calls_index;
    let fused_calls = calls_index;
    let saved_calls = separate_calls - fused_calls;
    assert_eq!(saved_calls, calls_partition + calls_replay);
    assert!(saved_calls > 0, "fusion must eliminate the replayed laps");
    let separate_bytes = bytes_partition + bytes_replay + bytes_index;
    assert!(
        separate_bytes > bytes_index,
        "separate laps must move more bytes than one fused pass"
    );
    let saved_bytes = separate_bytes - bytes_index;

    println!();
    println!("=== fused single-lap baseline probe (partition vs plan vs index) ===");
    println!(
        "fixture: {file_len} bytes, {record_count} fixed records x \
         {PROBE_RECORD_BYTES} B, parts {PROBE_PARTS}, stride {PROBE_STRIDE}, \
         scan_buf {PROBE_SCAN_BUF}"
    );
    println!(
        "partition == plan ranges: {}",
        ranges_partition == plan.ranges
    );
    println!(
        "index ranges: {} (record-aligned, cover file)",
        ranges_index.len()
    );
    println!(
        "post-target search bytes per partition lap: {post_target_sum} over {internal_cuts} cuts"
    );
    println!("replayed cut derivations (plan lap): {replayed_cuts}");
    println!(
        "pread calls: partition {calls_partition} + plan_replay {calls_replay} + \
         index_walk {calls_index} = separate {separate_calls}; fused {fused_calls}; \
         saved {saved_calls}"
    );
    println!(
        "pread bytes: partition {bytes_partition} + plan_replay {bytes_replay} + \
         index_walk {bytes_index} = separate {separate_bytes}; saved_vs_fused {saved_bytes}"
    );
    println!(
        "conclusion: single-lap fusion eliminates {saved_calls} syscalls and \
         {saved_bytes} bytes ({replayed_cuts} cut replays + {post_target_sum} \
         post-target bytes x2) on this fixture; index walk dominates and is shared"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
