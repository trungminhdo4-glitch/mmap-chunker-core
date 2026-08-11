use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use mmap_chunker_core::ffi::{
    mmap_engine_free, mmap_engine_get_chunk, mmap_engine_open, mmap_engine_partition_records,
    mmap_engine_scan_chunks_ex, mmap_engine_scan_chunks_pattern, mmap_engine_scan_fixed,
    CChunkView,
};
use mmap_chunker_core::MmapChunker;

#[derive(Debug, PartialEq, Eq)]
struct ObservedRange {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

enum PlanRequest {
    Single {
        chunk_size: usize,
        delimiter: u8,
    },
    Pattern {
        chunk_size: usize,
        delimiter: Vec<u8>,
    },
    Fixed {
        chunk_size: usize,
    },
    Partition {
        partitions: usize,
        delimiter: u8,
    },
}

fn fixture_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mmap_chunker_core_plan_parity_{}_{}",
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

fn rust_ranges(path: &Path, request: &PlanRequest) -> Vec<ObservedRange> {
    let mut chunker = unsafe { MmapChunker::open(path).unwrap() };
    let count = match request {
        PlanRequest::Single {
            chunk_size,
            delimiter,
        } => chunker.scan_delimited(*chunk_size, *delimiter),
        PlanRequest::Pattern {
            chunk_size,
            delimiter,
        } => chunker.scan_delimited_pattern(*chunk_size, delimiter),
        PlanRequest::Fixed { chunk_size } => chunker.scan_fixed(*chunk_size),
        PlanRequest::Partition {
            partitions,
            delimiter,
        } => chunker.partition_records(*partitions, *delimiter),
    };

    let source = chunker.as_bytes();
    let base = source.as_ptr();
    (0..count)
        .map(|index| {
            let chunk = chunker.get_chunk(index).unwrap();
            let start = unsafe { chunk.as_ptr().offset_from(base) as usize };
            let end = start + chunk.len();
            ObservedRange {
                start,
                end,
                bytes: chunk.to_vec(),
            }
        })
        .collect()
}

unsafe fn ffi_ranges(path: &Path, request: &PlanRequest) -> Vec<ObservedRange> {
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    let handle = mmap_engine_open(c_path.as_ptr().cast());
    assert!(!handle.is_null());

    let count = match request {
        PlanRequest::Single {
            chunk_size,
            delimiter,
        } => mmap_engine_scan_chunks_ex(handle, *chunk_size, *delimiter),
        PlanRequest::Pattern {
            chunk_size,
            delimiter,
        } => mmap_engine_scan_chunks_pattern(
            handle,
            *chunk_size,
            delimiter.as_ptr(),
            delimiter.len(),
        ),
        PlanRequest::Fixed { chunk_size } => mmap_engine_scan_fixed(handle, *chunk_size),
        PlanRequest::Partition {
            partitions,
            delimiter,
        } => mmap_engine_partition_records(handle, *partitions, *delimiter),
    };

    let mut views = Vec::with_capacity(count);
    for index in 0..count {
        let mut view = CChunkView {
            data: std::ptr::null(),
            len: 0,
        };
        assert_eq!(mmap_engine_get_chunk(handle, index, &mut view), 0);
        views.push(view);
    }

    let base = views.first().map_or(std::ptr::null(), |view| view.data);
    let ranges = views
        .into_iter()
        .map(|view| {
            let start = view.data.offset_from(base) as usize;
            let end = start + view.len;
            ObservedRange {
                start,
                end,
                bytes: std::slice::from_raw_parts(view.data, view.len).to_vec(),
            }
        })
        .collect();

    mmap_engine_free(handle);
    ranges
}

fn assert_parity(name: &str, data: &[u8], request: PlanRequest) {
    let path = write_fixture(name, data);
    let rust = rust_ranges(&path, &request);
    let ffi = unsafe { ffi_ranges(&path, &request) };

    assert_eq!(rust, ffi, "Rust/FFI range mismatch for fixture {name}");
    for range in rust {
        assert!(range.start <= range.end);
        assert_eq!(range.end - range.start, range.bytes.len());
    }

    fs::remove_file(path).unwrap();
}

#[test]
fn single_byte_plans_have_cross_surface_parity() {
    assert_parity(
        "single_newline",
        b"a\nb\nc\n",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "single_nul",
        b"a\0b\0c\0",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: 0,
        },
    );
    assert_parity(
        "single_absent",
        b"abcdef",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "single_no_trailing",
        b"a\nb\nc",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: b'\n',
        },
    );
}

#[test]
fn pattern_plans_have_cross_surface_parity() {
    assert_parity(
        "pattern_crlf",
        b"a\r\nb\r\nc",
        PlanRequest::Pattern {
            chunk_size: 2,
            delimiter: b"\r\n".to_vec(),
        },
    );
    assert_parity(
        "pattern_double_crlf",
        b"head\r\n\r\nbody\r\n\r\n",
        PlanRequest::Pattern {
            chunk_size: 4,
            delimiter: b"\r\n\r\n".to_vec(),
        },
    );
    assert_parity(
        "pattern_embedded_nul",
        b"a\0\0b\0\0c",
        PlanRequest::Pattern {
            chunk_size: 2,
            delimiter: b"\0\0".to_vec(),
        },
    );
    assert_parity(
        "pattern_absent",
        b"abcdef",
        PlanRequest::Pattern {
            chunk_size: 2,
            delimiter: b"\r\n".to_vec(),
        },
    );
    assert_parity(
        "pattern_longer_than_data",
        b"abc",
        PlanRequest::Pattern {
            chunk_size: 1,
            delimiter: b"abcdef".to_vec(),
        },
    );
    assert_parity(
        "pattern_empty_file",
        b"",
        PlanRequest::Pattern {
            chunk_size: 4,
            delimiter: b"\r\n".to_vec(),
        },
    );
}

#[test]
fn fixed_plans_have_cross_surface_parity() {
    assert_parity(
        "fixed_exact",
        b"12345678",
        PlanRequest::Fixed { chunk_size: 4 },
    );
    assert_parity(
        "fixed_remainder",
        b"1234567890",
        PlanRequest::Fixed { chunk_size: 4 },
    );
    assert_parity(
        "fixed_zero_size",
        b"12345",
        PlanRequest::Fixed { chunk_size: 0 },
    );
    assert_parity("fixed_empty", b"", PlanRequest::Fixed { chunk_size: 4 });
}

#[test]
fn partition_plans_have_cross_surface_parity() {
    assert_parity(
        "partition_one",
        b"a\nb\nc\n",
        PlanRequest::Partition {
            partitions: 1,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_many",
        b"a\nb\nc\nd\ne\n",
        PlanRequest::Partition {
            partitions: 3,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_giant_record",
        b"giant record without boundary until here\nsmall\n",
        PlanRequest::Partition {
            partitions: 8,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_no_delimiter",
        b"abcdef",
        PlanRequest::Partition {
            partitions: 4,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_fewer_records",
        b"a\nb\n",
        PlanRequest::Partition {
            partitions: 8,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_empty_file",
        b"",
        PlanRequest::Partition {
            partitions: 4,
            delimiter: b'\n',
        },
    );
}
