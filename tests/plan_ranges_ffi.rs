//! FFI contract tests for `mmap_engine_plan_partition_ranges` (ABI v1.5).
//!
//! The happy-path parity suites (`plan_parity`, `cli_partition`, the
//! `source` unit tests) drive successful plans. These tests pin the
//! two-phase protocol edges instead: capacity query, insufficient
//! capacity output preservation, invalid modes and pointers,
//! minimum-window enforcement, empty files, repeated-call stability,
//! and concurrent planning across backends.

use std::ffi::{CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};

use mmap_chunker_core::ffi::{
    mmap_engine_last_error, mmap_engine_plan_partition_ranges, CPartitionRange, SOURCE_MODE_MMAP,
    SOURCE_MODE_PREAD, SOURCE_MODE_WINDOWED,
};
use mmap_chunker_core::source::{plan_partition_ranges, PlannerOptions, SourceMode};

const MIN_WINDOW: usize = 65536;

fn fixture_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mmap_chunker_core_plan_ranges_ffi_{}_{}",
        std::process::id(),
        name
    ))
}

fn write_fixture(name: &str, data: &[u8]) -> PathBuf {
    let path = fixture_path(name);
    let _ = fs::remove_file(&path);
    fs::write(&path, data).unwrap();
    path
}

fn remove_fixture(path: &Path) {
    let _ = fs::remove_file(path);
}

fn last_error() -> String {
    let ptr = mmap_engine_last_error();
    assert!(!ptr.is_null(), "error pointer must never be null");
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

fn c_path(path: &Path) -> CString {
    CString::new(path.to_str().unwrap()).unwrap()
}

/// Drive the documented two-phase protocol and return the filled ranges.
fn plan_via_ffi(
    path: &Path,
    parts: usize,
    delimiter: &[u8],
    mode: u32,
    window_bytes: usize,
) -> Vec<(usize, usize)> {
    let path = c_path(path);
    let mut count: usize = 0;
    // Phase 1: query.
    let query = unsafe {
        mmap_engine_plan_partition_ranges(
            path.as_ptr(),
            parts,
            delimiter.as_ptr(),
            delimiter.len(),
            mode,
            window_bytes,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(query, -2, "query phase must report required capacity");
    let mut out: Vec<CPartitionRange> = (0..count)
        .map(|_| CPartitionRange {
            start: usize::MAX,
            end: usize::MAX,
        })
        .collect();
    // Phase 2: fill.
    let fill = unsafe {
        mmap_engine_plan_partition_ranges(
            path.as_ptr(),
            parts,
            delimiter.as_ptr(),
            delimiter.len(),
            mode,
            window_bytes,
            out.as_mut_ptr(),
            out.len(),
            &mut count,
        )
    };
    assert_eq!(fill, 0, "fill phase must succeed: {}", last_error());
    assert_eq!(count, out.len());
    out.into_iter().map(|r| (r.start, r.end)).collect()
}

fn rust_reference(
    path: &Path,
    parts: usize,
    delimiter: &[u8],
    mode: SourceMode,
) -> Vec<(usize, usize)> {
    let options = PlannerOptions::new(mode).with_window_bytes(MIN_WINDOW);
    unsafe { plan_partition_ranges(path, parts, delimiter, &options).unwrap() }
}

fn assert_full_coverage(ranges: &[(usize, usize)], file_len: usize) {
    assert!(!ranges.is_empty(), "dense fixture must yield ranges");
    assert_eq!(ranges[0].0, 0, "first range must start at 0");
    assert_eq!(
        ranges[ranges.len() - 1].1,
        file_len,
        "last range must end at EOF"
    );
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].1, pair[1].0, "ranges must be contiguous");
        assert!(pair[0].0 < pair[0].1, "ranges must be non-empty");
    }
}

fn dense_fixture() -> Vec<u8> {
    (0..64)
        .map(|i| format!("rec-{i:02}\n"))
        .collect::<String>()
        .into_bytes()
}

#[test]
fn two_phase_query_then_fill_matches_rust_reference() {
    let data = dense_fixture();
    let path = write_fixture("two-phase", &data);
    for (mode_u32, mode_rs) in [
        (SOURCE_MODE_MMAP, SourceMode::Mmap),
        (SOURCE_MODE_WINDOWED, SourceMode::Windowed),
        (SOURCE_MODE_PREAD, SourceMode::Pread),
    ] {
        let via_ffi = plan_via_ffi(&path, 8, b"\n", mode_u32, MIN_WINDOW);
        let reference = rust_reference(&path, 8, b"\n", mode_rs);
        assert_eq!(via_ffi, reference, "FFI must match Rust reference");
        assert_full_coverage(&via_ffi, data.len());
    }
    remove_fixture(&path);
}

#[test]
fn insufficient_capacity_preserves_output_and_reports_count() {
    let data = dense_fixture();
    let path = write_fixture("short-capacity", &data);
    let cpath = c_path(&path);
    let mut count: usize = usize::MAX;
    // Sentinel-filled buffer: must be bit-identical after the -2 return.
    let mut out: Vec<CPartitionRange> = (0..7)
        .map(|_| CPartitionRange {
            start: 0xDEAD_BEEF,
            end: 0xCAFE_F00D,
        })
        .collect();
    let code = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_PREAD,
            MIN_WINDOW,
            out.as_mut_ptr(),
            out.len(),
            &mut count,
        )
    };
    assert_eq!(code, -2, "short capacity must return -2");
    assert_eq!(count, 8, "*out_count must hold the required count");
    for range in &out {
        assert_eq!(range.start, 0xDEAD_BEEF);
        assert_eq!(range.end, 0xCAFE_F00D);
    }
    // Retry with the reported capacity succeeds.
    let mut full: Vec<CPartitionRange> = (0..count)
        .map(|_| CPartitionRange { start: 0, end: 0 })
        .collect();
    let retry = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_PREAD,
            MIN_WINDOW,
            full.as_mut_ptr(),
            full.len(),
            &mut count,
        )
    };
    assert_eq!(retry, 0, "retry must succeed: {}", last_error());
    assert_eq!(count, 8);
    assert_full_coverage(
        &full.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(),
        data.len(),
    );
    remove_fixture(&path);
}

#[test]
fn invalid_source_mode_rejected_without_touching_count() {
    let data = dense_fixture();
    let path = write_fixture("bad-mode", &data);
    let cpath = c_path(&path);
    for bad_mode in [3u32, 7, 99, u32::MAX] {
        let mut count: usize = 0x1234_5678;
        let code = unsafe {
            mmap_engine_plan_partition_ranges(
                cpath.as_ptr(),
                8,
                b"\n".as_ptr(),
                1,
                bad_mode,
                MIN_WINDOW,
                std::ptr::null_mut(),
                0,
                &mut count,
            )
        };
        assert_eq!(code, -1, "mode {bad_mode} must be rejected");
        assert_eq!(count, 0x1234_5678, "*out_count must be untouched");
        assert!(
            !last_error().is_empty(),
            "diagnostic must be set for mode {bad_mode}"
        );
    }
    remove_fixture(&path);
}

#[test]
fn null_and_empty_arguments_rejected() {
    let data = dense_fixture();
    let path = write_fixture("null-args", &data);
    let cpath = c_path(&path);
    let mut count: usize = 0;

    let null_path = unsafe {
        mmap_engine_plan_partition_ranges(
            std::ptr::null(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_MMAP,
            MIN_WINDOW,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(null_path, -1);
    assert!(!last_error().is_empty());

    let null_count = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_MMAP,
            MIN_WINDOW,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(null_count, -1);
    assert!(!last_error().is_empty());

    let null_delim = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            std::ptr::null(),
            1,
            SOURCE_MODE_MMAP,
            MIN_WINDOW,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(null_delim, -1);

    let empty_delim = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            0,
            SOURCE_MODE_MMAP,
            MIN_WINDOW,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(empty_delim, -1);

    let null_out_with_capacity = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_MMAP,
            MIN_WINDOW,
            std::ptr::null_mut(),
            4,
            &mut count,
        )
    };
    assert_eq!(null_out_with_capacity, -1);

    let zero_parts = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            0,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_MMAP,
            MIN_WINDOW,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(zero_parts, -1);
    assert!(last_error().contains("requested_partitions"));
    remove_fixture(&path);
}

#[test]
fn empty_file_returns_zero_ranges() {
    let path = write_fixture("empty", b"");
    let cpath = c_path(&path);
    let mut count: usize = usize::MAX;
    // No query phase exists for zero ranges: the call succeeds at once.
    let code = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_WINDOWED,
            MIN_WINDOW,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(code, 0);
    assert_eq!(count, 0);
    remove_fixture(&path);
}

#[test]
fn windowed_minimum_window_enforced_and_mmap_ignores_window() {
    let data = dense_fixture();
    let path = write_fixture("window-floor", &data);
    let cpath = c_path(&path);
    let mut count: usize = 0;
    let small = unsafe {
        mmap_engine_plan_partition_ranges(
            cpath.as_ptr(),
            8,
            b"\n".as_ptr(),
            1,
            SOURCE_MODE_WINDOWED,
            1024,
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(small, -1, "window < 64 KiB must be rejected");
    assert!(!last_error().is_empty());

    // Other backends ignore window_bytes entirely.
    let ranges = plan_via_ffi(&path, 8, b"\n", SOURCE_MODE_MMAP, 1);
    assert_full_coverage(&ranges, data.len());
    let ranges = plan_via_ffi(&path, 8, b"\n", SOURCE_MODE_PREAD, 1);
    assert_full_coverage(&ranges, data.len());
    remove_fixture(&path);
}

#[test]
fn repeated_calls_are_stable() {
    let data = dense_fixture();
    let path = write_fixture("repeated", &data);
    let first = plan_via_ffi(&path, 8, b"\n", SOURCE_MODE_WINDOWED, MIN_WINDOW);
    let second = plan_via_ffi(&path, 8, b"\n", SOURCE_MODE_WINDOWED, MIN_WINDOW);
    assert_eq!(first, second);
    remove_fixture(&path);
}

#[test]
fn giant_record_and_window_straddling_delimiter_match_reference() {
    // A 200 KiB record crosses several 64 KiB windows; the multi-byte
    // delimiter is placed to straddle window edges.
    let mut data = vec![b'x'; 200 * 1024];
    data.extend_from_slice(b"|||\n");
    data.extend_from_slice(&vec![b'y'; 100 * 1024]);
    data.extend_from_slice(b"|||\n");
    data.extend_from_slice(b"tail-no-newline");
    let path = write_fixture("giant-straddle", &data);
    for (mode_u32, mode_rs) in [
        (SOURCE_MODE_MMAP, SourceMode::Mmap),
        (SOURCE_MODE_WINDOWED, SourceMode::Windowed),
        (SOURCE_MODE_PREAD, SourceMode::Pread),
    ] {
        let via_ffi = plan_via_ffi(&path, 7, b"|||", mode_u32, MIN_WINDOW);
        let reference = rust_reference(&path, 7, b"|||", mode_rs);
        assert_eq!(via_ffi, reference, "giant/straddle parity");
        assert_full_coverage(&via_ffi, data.len());
    }
    remove_fixture(&path);
}

#[test]
fn concurrent_planning_across_modes_agrees() {
    let data = dense_fixture();
    let path = write_fixture("concurrent", &data);
    let expected = rust_reference(&path, 8, b"\n", SourceMode::Mmap);
    let mut handles = Vec::new();
    for thread in 0..8usize {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let mode = match thread % 3 {
                0 => (SOURCE_MODE_MMAP, SourceMode::Mmap),
                1 => (SOURCE_MODE_WINDOWED, SourceMode::Windowed),
                _ => (SOURCE_MODE_PREAD, SourceMode::Pread),
            };
            let via_ffi = plan_via_ffi(&path, 8, b"\n", mode.0, MIN_WINDOW);
            let via_rust = rust_reference(&path, 8, b"\n", mode.1);
            assert_eq!(via_ffi, via_rust);
            via_ffi
        }));
    }
    for handle in handles {
        assert_eq!(handle.join().unwrap(), expected);
    }
    remove_fixture(&path);
}
