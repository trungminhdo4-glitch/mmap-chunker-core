//! Performance baseline: mmap chunker vs std::fs::read.
//!
//! Run manually:  cargo test --test benchmark -- --nocapture --ignored
//!
//! Creates temp files, runs multiple iterations, reports wall-clock and
//! throughput.  No external dependencies — std only.

use std::ffi::CString;
use std::time::Instant;

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
