//! Integration test for the C-ABI layer.
//!
//! Creates a temporary file (> 1 MB), opens it via the C ABI, scans chunks,
//! and verifies that chunk data pointers point directly into the memory-mapped
//! file (zero-copy proof).

use std::ffi::CString;
use std::io::Write;

use mmap_chunker_core::{CChunkView, CEngineHandle};

extern "C" {
    fn mmap_engine_open(path: *const std::ffi::c_char) -> *mut CEngineHandle;
    fn mmap_engine_scan_chunks(handle: *mut CEngineHandle, chunk_size_bytes: usize) -> usize;
    fn mmap_engine_partition_records(
        handle: *mut CEngineHandle,
        requested_partitions: usize,
        delimiter: u8,
    ) -> usize;
    fn mmap_engine_get_chunk(handle: *mut CEngineHandle, index: usize, out: *mut CChunkView)
        -> i32;
    fn mmap_engine_free(handle: *mut CEngineHandle);
}

#[test]
fn test_c_abi_zero_copy_chunking() {
    let dir = std::env::temp_dir().join("mmap_chunker_core_c_abi_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("test_data.csvl");

    let line_len = 100;
    let line_data = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789,val1,val2,val3,val4,val5,val6,val7,val8,val9,val10,val11,val12\n";
    assert_eq!(
        line_data.len(),
        line_len,
        "test line must be exactly 100 bytes"
    );

    let total_lines = 15_000usize;
    {
        let mut file = std::fs::File::create(&file_path).unwrap();
        for _ in 0..total_lines {
            file.write_all(line_data).unwrap();
        }
        file.flush().unwrap();
    }

    let expected_content = std::fs::read(&file_path).unwrap();
    assert_eq!(
        expected_content.len(),
        total_lines * line_len,
        "file size mismatch"
    );
    assert!(
        expected_content.len() > 1_000_000,
        "file must be > 1 MB for the test"
    );

    let c_path = CString::new(file_path.to_str().unwrap()).unwrap();
    let chunk_size = 128 * 1024;

    unsafe {
        let handle = mmap_engine_open(c_path.as_ptr());
        assert!(!handle.is_null(), "mmap_engine_open returned null");

        let chunk_count = mmap_engine_scan_chunks(handle, chunk_size);
        assert!(chunk_count > 0, "expected at least one chunk");
        assert!(
            chunk_count > 1,
            "expected multiple chunks with ~128 KB size on 1.5 MB file"
        );

        let mut view = CChunkView {
            data: std::ptr::null(),
            len: 0,
        };
        let mut pos = 0usize;

        for i in 0..chunk_count {
            let ret = mmap_engine_get_chunk(handle, i, &mut view);
            assert_eq!(ret, 0, "mmap_engine_get_chunk failed at index {}", i);
            assert!(
                !view.data.is_null(),
                "chunk data pointer is null at index {}",
                i
            );
            assert!(view.len > 0, "chunk length is zero at index {}", i);

            let chunk_slice = std::slice::from_raw_parts(view.data, view.len);
            assert_eq!(
                chunk_slice,
                &expected_content[pos..pos + view.len],
                "chunk {} data mismatch: zero-copy pointer does not match file content",
                i
            );

            pos += view.len;
        }

        assert_eq!(
            pos,
            expected_content.len(),
            "chunks did not cover the entire file"
        );

        let out_of_bounds_ret = mmap_engine_get_chunk(handle, chunk_count, &mut view);
        assert_eq!(
            out_of_bounds_ret, -1,
            "out-of-bounds index should return -1"
        );

        mmap_engine_free(handle);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_c_abi_edge_cases() {
    let dir = std::env::temp_dir().join("mmap_chunker_core_c_abi_edge");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let test_no_trailing_nl = dir.join("no_trailing_nl.txt");
    std::fs::write(&test_no_trailing_nl, b"line1\nline2\nline3").unwrap();

    let test_no_nl = dir.join("no_nl.txt");
    std::fs::write(&test_no_nl, b"no_newlines_at_all").unwrap();

    unsafe {
        let view = &mut CChunkView {
            data: std::ptr::null(),
            len: 0,
        };

        let c_path = CString::new(test_no_trailing_nl.to_str().unwrap()).unwrap();
        let h = mmap_engine_open(c_path.as_ptr());
        assert!(!h.is_null());

        let count = mmap_engine_scan_chunks(h, 4);
        assert!(count > 0);

        let mut total = 0usize;
        for i in 0..count {
            let ret = mmap_engine_get_chunk(h, i, view);
            assert_eq!(ret, 0);
            total += view.len;
        }
        assert_eq!(total, 17);

        let count2 = mmap_engine_scan_chunks(h, 64);
        assert_eq!(count2, 1);

        mmap_engine_free(h);

        let c_path2 = CString::new(test_no_nl.to_str().unwrap()).unwrap();
        let h2 = mmap_engine_open(c_path2.as_ptr());
        assert!(!h2.is_null());

        let count3 = mmap_engine_scan_chunks(h2, 4);
        assert_eq!(count3, 1);
        let ret = mmap_engine_get_chunk(h2, 0, view);
        assert_eq!(ret, 0);
        assert_eq!(view.len, 18);

        mmap_engine_free(h2);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_c_abi_exact_record_boundaries_produce_four_ranges() {
    let dir = std::env::temp_dir().join("mmap_chunker_core_c_abi_exact_boundaries");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("four-records.txt");
    std::fs::write(&file_path, b"a\nb\nc\nd\n").unwrap();

    let c_path = CString::new(file_path.to_str().unwrap()).unwrap();
    let expected: [&[u8]; 4] = [b"a\n", b"b\n", b"c\n", b"d\n"];

    unsafe {
        let handle = mmap_engine_open(c_path.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(mmap_engine_partition_records(handle, 4, b'\n'), 4);

        for (index, expected_chunk) in expected.iter().enumerate() {
            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            assert_eq!(mmap_engine_get_chunk(handle, index, &mut view), 0);
            assert_eq!(
                std::slice::from_raw_parts(view.data, view.len),
                *expected_chunk
            );
        }

        mmap_engine_free(handle);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
