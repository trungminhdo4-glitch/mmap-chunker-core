//! C-ABI export layer.
//!
//! Provides an opaque engine handle and C-callable functions for
//! memory-mapping files, scanning chunk boundaries, and retrieving
//! zero-copy chunk views.
//!
//! # ABI versioning
//!
//! Use `mmap_engine_abi_version()` for runtime version discovery and
//! `mmap_engine_capabilities()` for feature detection. Additive API
//! additions (new functions, new capability bits) do not break ABI.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr};

use crate::mmap::MmapFile;
use crate::scanner;

// ─── ABI constants ────────────────────────────────────────────────────────────

pub const ABI_VERSION: u32 = 0x0001_0003;

pub const CAP_ZERO_COPY: u32 = 1 << 0;
pub const CAP_CONFIGURABLE_DELIMITER: u32 = 1 << 1;
pub const CAP_ERROR_STRINGS: u32 = 1 << 2;
pub const CAP_FIXED_SIZE_CHUNKING: u32 = 1 << 3;
pub const CAP_RECORD_PARTITIONING: u32 = 1 << 4;
pub const CAP_MULTI_BYTE_DELIMITER: u32 = 1 << 5;

const MAX_ERROR_LEN: usize = 256;

thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static LAST_ERROR: RefCell<[u8; MAX_ERROR_LEN]> = RefCell::new([0u8; MAX_ERROR_LEN]);
}

fn set_error(msg: &str) {
    LAST_ERROR.with(|cell| {
        let mut buf = cell.borrow_mut();
        let src = msg.as_bytes();
        let len = src.len().min(MAX_ERROR_LEN - 1);
        buf[..len].copy_from_slice(&src[..len]);
        buf[len] = 0;
    });
}

fn clear_error() {
    LAST_ERROR.with(|cell| {
        cell.borrow_mut()[0] = 0;
    });
}

// ─── C-compatible types ───────────────────────────────────────────────────────

/// A view into a single chunk.
///
/// The `data` pointer points directly into the memory-mapped file
/// and remains valid until `mmap_engine_free` is called.
#[repr(C)]
pub struct CChunkView {
    pub data: *const u8,
    pub len: usize,
}

/// Opaque engine handle.
///
/// Allocated by `mmap_engine_open` and must be freed with `mmap_engine_free`.
/// The C side sees this as an opaque pointer; the layout is internal.
#[repr(C)]
pub struct CEngineHandle {
    _private: u8,
}

// ─── Internal engine state ────────────────────────────────────────────────────

enum ChunkLayout {
    Empty,
    Delimited(Vec<(usize, usize)>),
    Fixed {
        chunk_size: usize,
        chunk_count: usize,
    },
    Partitioned(Vec<(usize, usize)>),
}

struct Engine {
    mmap: MmapFile,
    layout: ChunkLayout,
}

// ─── ABI discovery ────────────────────────────────────────────────────────────

/// Return the ABI version as `(major << 16) | minor`.
///
/// Current: `0x0001_0003` (v1.3). Always succeeds, never panics.
#[no_mangle]
pub extern "C" fn mmap_engine_abi_version() -> u32 {
    ABI_VERSION
}

/// Return a bitmask of supported capabilities.
///
/// Consumers call this once at load time to discover which optional
/// features the loaded library provides.
///
/// Current bits (v1.3):
///   - Bit 0: `ZERO_COPY` — chunk views reference mapped memory directly
///   - Bit 1: `CONFIGURABLE_DELIMITER` — `mmap_engine_scan_chunks_ex` available
///   - Bit 2: `ERROR_STRINGS` — `mmap_engine_last_error` returns diagnostic text
///   - Bit 3: `FIXED_SIZE_CHUNKING` — `mmap_engine_scan_fixed` available
///   - Bit 4: `RECORD_PARTITIONING` — `mmap_engine_partition_records` available
///   - Bit 5: `MULTI_BYTE_DELIMITER` — `mmap_engine_scan_chunks_pattern` available
#[no_mangle]
pub extern "C" fn mmap_engine_capabilities() -> u32 {
    CAP_ZERO_COPY
        | CAP_CONFIGURABLE_DELIMITER
        | CAP_ERROR_STRINGS
        | CAP_FIXED_SIZE_CHUNKING
        | CAP_RECORD_PARTITIONING
        | CAP_MULTI_BYTE_DELIMITER
}

/// Return a pointer to the last error message for the calling thread,
/// or NULL if no error occurred.
///
/// The returned pointer references an internal thread-local buffer
/// (max 255 chars + NUL). It remains valid until the next call to any
/// API function on the same thread. The caller must copy the string
/// if it needs to persist beyond the next API call.
///
/// Thread-safe: each thread has its own error buffer.
#[no_mangle]
pub extern "C" fn mmap_engine_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| {
        let buf = cell.borrow();
        if buf[0] == 0 {
            std::ptr::null()
        } else {
            buf.as_ptr() as *const c_char
        }
    })
}

// ─── Core FFI ─────────────────────────────────────────────────────────────────

/// Open and memory-map a file for chunked access.
///
/// `path` must be a null-terminated UTF-8 C string.
///
/// Returns a heap-allocated opaque engine handle on success, or null if the
/// file cannot be opened or mapped. On failure, call `mmap_engine_last_error()`
/// for a diagnostic message.
///
/// # Safety
///
/// `path` must point to a valid null-terminated C string. The returned
/// handle must be freed with `mmap_engine_free` exactly once.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_open(path: *const c_char) -> *mut CEngineHandle {
    let inner = move || {
        clear_error();

        if path.is_null() {
            set_error("path is null");
            return std::ptr::null_mut();
        }

        let c_str = unsafe { CStr::from_ptr(path) };

        match unsafe { MmapFile::open(c_str) } {
            Some(mmap) => {
                mmap.advise_sequential();
                let engine = Box::new(Engine {
                    mmap,
                    layout: ChunkLayout::Empty,
                });
                Box::into_raw(engine) as *mut CEngineHandle
            }
            None => {
                set_error("failed to open or map file");
                std::ptr::null_mut()
            }
        }
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(ptr) => ptr,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_open");
            std::ptr::null_mut()
        }
    }
}

/// Scan the mapped file for chunk boundaries using newline (`\n`, 0x0A)
/// as the delimiter.
///
/// This is the original v1.0 API preserved for backward compatibility.
/// New consumers should prefer `mmap_engine_scan_chunks_ex` which
/// supports configurable delimiters.
///
/// Returns the number of chunks found, or 0 on error (null handle,
/// empty file, or internal failure). On error, call
/// `mmap_engine_last_error()` for a diagnostic message.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open`
/// and must not have been freed.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_scan_chunks(
    handle: *mut CEngineHandle,
    chunk_size_bytes: usize,
) -> usize {
    unsafe { mmap_engine_scan_chunks_ex(handle, chunk_size_bytes, b'\n') }
}

/// Scan the mapped file for chunk boundaries.
///
/// Chunks are created at approximately `chunk_size_bytes` intervals.
/// Each chunk boundary is placed immediately after a `delimiter` byte
/// found at or after the target offset. The last chunk extends to the
/// end of the file.
///
/// Returns the number of chunks found, or 0 on error (null handle,
/// empty file, or internal failure). On error, call
/// `mmap_engine_last_error()` for a diagnostic message.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open`
/// and must not have been freed.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_scan_chunks_ex(
    handle: *mut CEngineHandle,
    chunk_size_bytes: usize,
    delimiter: u8,
) -> usize {
    let inner = move || {
        clear_error();

        if handle.is_null() {
            set_error("handle is null");
            return 0;
        }

        let engine = unsafe { &mut *(handle as *mut Engine) };

        let data = unsafe { engine.mmap.as_slice() };
        if data.is_empty() {
            engine.layout = ChunkLayout::Empty;
            return 0;
        }

        let chunks = scanner::find_chunk_boundaries(data, chunk_size_bytes, delimiter);
        let count = chunks.len();
        engine.layout = ChunkLayout::Delimited(chunks);
        count
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(count) => count,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_scan_chunks");
            0
        }
    }
}

/// Scan the mapped file for chunk boundaries using a borrowed byte pattern.
///
/// Chunks are created at approximately `chunk_size_bytes` intervals. Each
/// boundary is placed immediately after the complete `delimiter` pattern
/// found at or after the target offset. The last chunk extends to EOF.
///
/// The delimiter is borrowed only for the duration of this call. The engine
/// stores only the resulting chunk boundaries, so the caller may release or
/// reuse the delimiter buffer after the function returns.
///
/// `delimiter_len` must be greater than zero. A null `delimiter` is invalid;
/// embedded NUL bytes are allowed because the delimiter is length-delimited.
/// The caller must ensure that `delimiter` points to at least
/// `delimiter_len` readable bytes for the duration of this call.
///
/// Returns the number of chunks found, or 0 on invalid input, empty file, or
/// internal failure. Invalid input and internal failure leave the previous
/// valid layout unchanged. On error, call `mmap_engine_last_error()`.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open` and must
/// not have been freed. If `delimiter_len` is non-zero, `delimiter` must be
/// non-null and point to `delimiter_len` readable bytes for the duration of
/// this call. The delimiter memory must not be mutated concurrently.
///
/// Threading: Same contract as `mmap_engine_scan_chunks()`.
/// Added in ABI v1.3 (detect with `MMAP_ENGINE_CAP_MULTI_BYTE_DELIMITER`).
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_scan_chunks_pattern(
    handle: *mut CEngineHandle,
    chunk_size_bytes: usize,
    delimiter: *const u8,
    delimiter_len: usize,
) -> usize {
    let inner = move || {
        clear_error();

        if handle.is_null() {
            set_error("handle is null");
            return 0;
        }

        if delimiter_len == 0 {
            set_error("delimiter_len must be > 0");
            return 0;
        }

        if delimiter.is_null() {
            set_error("delimiter is null");
            return 0;
        }

        if delimiter_len > isize::MAX as usize {
            set_error("delimiter_len exceeds supported range");
            return 0;
        }

        let engine = unsafe { &mut *(handle as *mut Engine) };
        let data = unsafe { engine.mmap.as_slice() };

        // SAFETY: the caller guarantees that `delimiter` points to
        // `delimiter_len` readable, immutable bytes for this call. The
        // slice is used only to compute boundaries and is never stored.
        let delimiter = unsafe { std::slice::from_raw_parts(delimiter, delimiter_len) };
        let chunks = scanner::find_chunk_boundaries_pattern(data, chunk_size_bytes, delimiter);
        let count = chunks.len();
        engine.layout = ChunkLayout::Delimited(chunks);
        count
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(count) => count,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_scan_chunks_pattern");
            0
        }
    }
}

/// Scan the mapped file into sequential fixed-size chunks.
///
/// Chunks are created at exact `chunk_size_bytes` intervals, with the last
/// chunk potentially shorter at EOF. No delimiter semantics — this mode is
/// suitable for binary/non-record workloads.
///
/// `chunk_size_bytes` of 0 is silently clamped to 1, consistent with all
/// other scan functions.
///
/// Calling this function replaces any previously computed chunk boundaries.
///
/// Returns the number of chunks found, or 0 on error (null handle, empty file).
/// On error, call `mmap_engine_last_error()` for diagnostics.
///
/// Threading: Same contract as `mmap_engine_scan_chunks()`.
/// Added in ABI v1.1 (detect with `MMAP_ENGINE_CAP_FIXED_SIZE_CHUNKING`).
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open`
/// and must not have been freed.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_scan_fixed(
    handle: *mut CEngineHandle,
    chunk_size_bytes: usize,
) -> usize {
    let inner = move || {
        clear_error();

        if handle.is_null() {
            set_error("handle is null");
            return 0;
        }

        let engine = unsafe { &mut *(handle as *mut Engine) };

        let file_len = engine.mmap.len();
        if file_len == 0 {
            engine.layout = ChunkLayout::Empty;
            return 0;
        }

        let effective_size = chunk_size_bytes.max(1);
        let count = file_len.div_ceil(effective_size);
        engine.layout = ChunkLayout::Fixed {
            chunk_size: effective_size,
            chunk_count: count,
        };
        count
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(count) => count,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_scan_fixed");
            0
        }
    }
}

/// Plan record-aligned partition byte ranges for N-way parallel consumers.
///
/// Computes approximately balanced, record-aligned byte ranges partitioning
/// the memory-mapped file into `requested_partitions` contiguous segments.
/// Each segment boundary falls on a record boundary (immediately after the
/// `delimiter` byte), ensuring no record is split.
///
/// Uses absolute ideal cut points: for each boundary `i = 1..N-1`, the
/// target position `floor(file_len * i / N)` is computed independently,
/// then forward-searched to the next delimiter. This prevents cumulative
/// drift that iterative approaches suffer from.
///
/// # Properties
///
/// - Complete coverage: `first.start == 0`, `last.end == file_len`
/// - No gaps, no overlaps — contiguous byte ranges
/// - Record integrity: non-final partitions end immediately after `delimiter`
/// - Deterministic: same input always produces the same result
/// - Partition sizes approximate `file_len / actual_count`
/// - `O(N)` metadata, bounded byte scanning (≤ file_len total)
/// - No full-file sequential scan required
///
/// # Edge cases
///
/// | Case | Result |
/// |------|--------|
/// | `handle` is NULL | Returns 0 + error |
/// | `requested_partitions == 0` | Returns 0 + error |
/// | Empty file | Returns 0 (no partitions) |
/// | `requested_partitions == 1` | Returns 1 (whole file) |
/// | No delimiter in file | Returns 1 (whole file) |
/// | Giant record spans multiple targets | Boundaries collapse, actual < requested |
/// | Fewer records than requested | Produces ≤ record count partitions |
///
/// # Return value
///
/// The actual number of partitions, which may be less than `requested_partitions`
/// if records are sparse. Returns 0 on error; call `mmap_engine_last_error()` for
/// diagnostics.
///
/// # Mode switching
///
/// Replaces any previous layout (delimited, fixed, or partitioned). Call
/// `mmap_engine_get_chunk()` afterwards to retrieve partitions zero-copy.
///
/// # Threading
///
/// Single-thread open/scan, multi-thread get_chunk after scan completes.
/// Same contract as `mmap_engine_scan_chunks()`.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open`
/// and must not have been freed.
///
/// Added in ABI v1.2 (detect with `MMAP_ENGINE_CAP_RECORD_PARTITIONING`).
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_partition_records(
    handle: *mut CEngineHandle,
    requested_partitions: usize,
    delimiter: u8,
) -> usize {
    let inner = move || {
        clear_error();

        if handle.is_null() {
            set_error("handle is null");
            return 0;
        }

        let engine = unsafe { &mut *(handle as *mut Engine) };

        let file_len = engine.mmap.len();
        if file_len == 0 {
            engine.layout = ChunkLayout::Empty;
            return 0;
        }

        if requested_partitions == 0 {
            set_error("requested_partitions must be > 0");
            return 0;
        }

        let data = unsafe { engine.mmap.as_slice() };
        let partitions = scanner::find_partition_boundaries(data, requested_partitions, delimiter);

        let count = partitions.len();
        engine.layout = ChunkLayout::Partitioned(partitions);
        count
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(count) => count,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_partition_records");
            0
        }
    }
}

/// Retrieve a chunk view by index.
///
/// Writes the chunk's data pointer and length into `out_chunk`. The
/// pointer points directly into the memory-mapped file (zero-copy) and
/// remains valid until `mmap_engine_free` is called.
///
/// Returns 0 on success, -1 if `handle` or `out_chunk` is null, or if
/// `index` is out of bounds. On error, call `mmap_engine_last_error()`
/// for a diagnostic message.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open`.
/// `out_chunk` must be a valid, aligned, writable pointer to a `CChunkView`.
/// `mmap_engine_scan_chunks` must have been called first.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_get_chunk(
    handle: *mut CEngineHandle,
    index: usize,
    out_chunk: *mut CChunkView,
) -> c_int {
    let inner = move || {
        clear_error();

        if handle.is_null() {
            set_error("handle is null");
            return -1;
        }

        if out_chunk.is_null() {
            set_error("out_chunk is null");
            return -1;
        }

        let engine = unsafe { &*(handle as *mut Engine) };

        let (start, end) = match &engine.layout {
            ChunkLayout::Empty => {
                set_error("chunk index out of bounds");
                return -1;
            }
            ChunkLayout::Delimited(chunks) | ChunkLayout::Partitioned(chunks) => {
                if index >= chunks.len() {
                    set_error("chunk index out of bounds");
                    return -1;
                }
                chunks[index]
            }
            ChunkLayout::Fixed {
                chunk_size,
                chunk_count,
            } => {
                if index >= *chunk_count {
                    set_error("chunk index out of bounds");
                    return -1;
                }
                match scanner::fixed_chunk_bounds(engine.mmap.len(), *chunk_size, index) {
                    Some(bounds) => bounds,
                    None => {
                        set_error("internal error: fixed chunk bounds overflow");
                        return -1;
                    }
                }
            }
        };

        unsafe {
            (*out_chunk).data = engine.mmap.as_ptr().add(start);
            (*out_chunk).len = end - start;
        }

        0
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(ret) => ret,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_get_chunk");
            -1
        }
    }
}

/// Free the engine handle and release all resources.
///
/// After this call, the handle is invalid and chunk views obtained from
/// `mmap_engine_get_chunk` must no longer be used.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open` or null.
/// Passing null is a no-op. Must not be called more than once.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_free(handle: *mut CEngineHandle) {
    let inner = move || {
        if handle.is_null() {
            return;
        }

        // SAFETY: caller guarantees `handle` was allocated by
        // `mmap_engine_open` and has not yet been freed.
        let _ = unsafe { Box::from_raw(handle as *mut Engine) };
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner));
    if result.is_err() {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type PatternCase<'a> = (&'a str, &'a [u8], &'a [u8], usize, usize);

    #[test]
    fn test_null_handle_safety() {
        unsafe {
            assert!(mmap_engine_open(std::ptr::null()).is_null());
            assert_eq!(mmap_engine_scan_chunks(std::ptr::null_mut(), 1024), 0);
            assert_eq!(
                mmap_engine_get_chunk(std::ptr::null_mut(), 0, std::ptr::null_mut()),
                -1
            );
            mmap_engine_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn test_nonexistent_file_returns_null() {
        let path = c"/nonexistent/file";
        unsafe {
            let h = mmap_engine_open(path.as_ptr());
            assert!(h.is_null());
        }
    }

    #[test]
    fn test_zero_size_file_through_ffi() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_zero_ffi");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("empty.dat");
        std::fs::File::create(&file_path).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null(), "zero-size file should return a valid handle");
            let count = mmap_engine_scan_chunks(h, 1024);
            assert_eq!(count, 0, "zero-size file should produce 0 chunks");
            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_get_chunk_before_scan() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_prescan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"line1\nline2\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let ret = mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(ret, -1, "get_chunk before scan should return -1");

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_repeated_scan_through_ffi() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_repeated_scan_ffi");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"a\nb\nc\nd\ne\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count1 = mmap_engine_scan_chunks(h, 4);
            let count2 = mmap_engine_scan_chunks(h, 4);
            assert_eq!(count1, count2, "repeated scan should produce same count");
            assert!(count1 > 0);

            let count3 = mmap_engine_scan_chunks(h, 8);
            assert_ne!(count3, count1, "different chunk size should differ");

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_chunk_size_zero_through_ffi() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_zero_chunk_ffi");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"abc\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_chunks(h, 0);
            assert!(
                count > 0,
                "chunk_size=0 should clamp to 1 and produce chunks"
            );

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_abi_cchunkview_layout() {
        let view = CChunkView {
            data: std::ptr::null(),
            len: 0,
        };
        let view_ptr: *const CChunkView = &view;
        let data_ptr: *const *const u8 = &view.data;
        let len_ptr: *const usize = &view.len;

        assert_eq!(
            std::mem::size_of::<CChunkView>(),
            std::mem::size_of::<usize>() * 2,
            "CChunkView should be two pointer-sized fields"
        );

        assert_eq!(
            std::mem::align_of::<CChunkView>(),
            std::mem::align_of::<usize>()
        );

        let data_offset = data_ptr as usize - view_ptr as usize;
        let len_offset = len_ptr as usize - view_ptr as usize;
        assert_eq!(data_offset, 0, "data must be at offset 0");
        assert_eq!(
            len_offset,
            std::mem::size_of::<usize>(),
            "len must follow data"
        );
    }

    #[test]
    fn test_abi_engine_handle_is_opaque() {
        assert_eq!(std::mem::size_of::<CEngineHandle>(), 1);
        assert_eq!(
            std::mem::size_of::<*const CEngineHandle>(),
            std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn test_abi_version() {
        assert_eq!(mmap_engine_abi_version(), ABI_VERSION);
    }

    #[test]
    fn test_capabilities() {
        let caps = mmap_engine_capabilities();
        assert!(caps & CAP_ZERO_COPY != 0, "must have ZERO_COPY");
        assert!(
            caps & CAP_CONFIGURABLE_DELIMITER != 0,
            "must have CONFIGURABLE_DELIMITER"
        );
        assert!(caps & CAP_ERROR_STRINGS != 0, "must have ERROR_STRINGS");
        assert!(
            caps & CAP_FIXED_SIZE_CHUNKING != 0,
            "must have FIXED_SIZE_CHUNKING"
        );
        assert!(
            caps & CAP_RECORD_PARTITIONING != 0,
            "must have RECORD_PARTITIONING"
        );
        assert!(
            caps & CAP_MULTI_BYTE_DELIMITER != 0,
            "must have MULTI_BYTE_DELIMITER"
        );
    }

    #[test]
    fn test_last_error_null_path() {
        unsafe {
            let _ = mmap_engine_open(std::ptr::null());
            let err = mmap_engine_last_error();
            assert!(!err.is_null(), "should have error after null path");
            let msg = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
            assert!(msg.contains("null"));
        }
    }

    #[test]
    fn test_last_error_nonexistent_file() {
        let path = c"/nonexistent/path/for/error/test";
        unsafe {
            let h = mmap_engine_open(path.as_ptr());
            assert!(h.is_null());
            let err = mmap_engine_last_error();
            assert!(!err.is_null());
        }
    }

    #[test]
    fn test_last_error_cleared_on_success() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_error_clear");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"hello\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let _ = mmap_engine_open(std::ptr::null());
            assert!(!mmap_engine_last_error().is_null());

            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());
            assert!(
                mmap_engine_last_error().is_null(),
                "error should be cleared on success"
            );

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_ex_custom_delimiter() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_custom_delim");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.csv");
        std::fs::write(&file_path, b"a,b,c,d,e\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count_comma = mmap_engine_scan_chunks_ex(h, 1, b',');
            assert!(count_comma > 0, "comma delimiter should find chunks");

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let ret = mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(ret, 0);
            assert_eq!(view.len, 2);
            assert_eq!(std::slice::from_raw_parts(view.data, view.len), b"a,");

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_ex_equals_scan_chunks_for_newline() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_scan_ex_compat");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"aaa\nbbb\nccc\nddd\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h1 = mmap_engine_open(c_path.as_ptr());
            let h2 = mmap_engine_open(c_path.as_ptr());
            assert!(!h1.is_null());
            assert!(!h2.is_null());

            let c1 = mmap_engine_scan_chunks(h1, 4);
            let c2 = mmap_engine_scan_chunks_ex(h2, 4, b'\n');
            assert_eq!(c1, c2, "scan_chunks and scan_chunks_ex with \\n must match");

            let mut v1 = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let mut v2 = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            for i in 0..c1 {
                let r1 = mmap_engine_get_chunk(h1, i, &mut v1);
                let r2 = mmap_engine_get_chunk(h2, i, &mut v2);
                assert_eq!(r1, 0);
                assert_eq!(r2, 0);
                assert_eq!(v1.len, v2.len);
                assert_eq!(
                    std::slice::from_raw_parts(v1.data, v1.len),
                    std::slice::from_raw_parts(v2.data, v2.len)
                );
            }

            mmap_engine_free(h1);
            mmap_engine_free(h2);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_ex_normal() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_scan_ex_normal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"first\nsecond\nthird\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_chunks_ex(h, 1, b'\n');
            assert_eq!(count, 3);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 6);
            mmap_engine_get_chunk(h, 1, &mut view);
            assert_eq!(view.len, 7);
            mmap_engine_get_chunk(h, 2, &mut view);
            assert_eq!(view.len, 6);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn naive_pattern_boundaries(
        data: &[u8],
        chunk_size: usize,
        delimiter: &[u8],
    ) -> Vec<(usize, usize)> {
        assert!(!delimiter.is_empty());
        if data.is_empty() {
            return Vec::new();
        }

        let step = chunk_size.max(1);
        let mut chunks = Vec::new();
        let mut start = 0usize;
        while start < data.len() {
            let target = start.saturating_add(step);
            let end = if target >= data.len() {
                data.len()
            } else {
                let remainder = &data[target..];
                if remainder.len() < delimiter.len() {
                    data.len()
                } else {
                    let match_pos = (0..=remainder.len() - delimiter.len())
                        .find(|&pos| &remainder[pos..pos + delimiter.len()] == delimiter);
                    match_pos
                        .map(|pos| target + pos + delimiter.len())
                        .unwrap_or(data.len())
                }
            };
            chunks.push((start, end));
            start = end;
        }
        chunks
    }

    #[test]
    fn test_scan_chunks_pattern_crlf_binary_and_len_one_equivalence() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_pattern_ffi");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        let data = b"a\r\nb\r\nc\r\n";
        std::fs::write(&file_path, data).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let delimiter = vec![b'\r', b'\n'];
            let count = mmap_engine_scan_chunks_pattern(h, 4, delimiter.as_ptr(), delimiter.len());
            drop(delimiter);
            assert_eq!(count, 2);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(
                std::slice::from_raw_parts(view.data, view.len),
                b"a\r\nb\r\n"
            );
            assert_eq!(mmap_engine_get_chunk(h, 1, &mut view), 0);
            assert_eq!(std::slice::from_raw_parts(view.data, view.len), b"c\r\n");
            mmap_engine_free(h);
        }

        let binary_path = dir.join("binary.dat");
        let binary_data = b"AB\x00\xff\x00CD\x00\xff\x00EF";
        std::fs::write(&binary_path, binary_data).unwrap();
        let binary_c_path = std::ffi::CString::new(binary_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(binary_c_path.as_ptr());
            assert!(!h.is_null());
            let delimiter = [0x00, 0xff, 0x00];
            let count = mmap_engine_scan_chunks_pattern(h, 4, delimiter.as_ptr(), delimiter.len());
            assert_eq!(count, 2);
            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(
                std::slice::from_raw_parts(view.data, view.len),
                &binary_data[..10]
            );
            mmap_engine_free(h);
        }

        let h1 = unsafe { mmap_engine_open(c_path.as_ptr()) };
        let h2 = unsafe { mmap_engine_open(c_path.as_ptr()) };
        assert!(!h1.is_null() && !h2.is_null());
        unsafe {
            let newline = *b"\n";
            let count_ex = mmap_engine_scan_chunks_ex(h1, 4, b'\n');
            let count_pattern =
                mmap_engine_scan_chunks_pattern(h2, 4, newline.as_ptr(), newline.len());
            assert_eq!(count_ex, count_pattern);
            let mut v1 = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let mut v2 = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            for i in 0..count_ex {
                assert_eq!(mmap_engine_get_chunk(h1, i, &mut v1), 0);
                assert_eq!(mmap_engine_get_chunk(h2, i, &mut v2), 0);
                assert_eq!(v1.len, v2.len);
                assert_eq!(
                    std::slice::from_raw_parts(v1.data, v1.len),
                    std::slice::from_raw_parts(v2.data, v2.len)
                );
            }
            mmap_engine_free(h1);
            mmap_engine_free(h2);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_pattern_invalid_inputs_preserve_layout() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_pattern_invalid");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"aaa\nbbb\n").unwrap();
        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();

        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());
            let previous_count = mmap_engine_scan_chunks_ex(h, 4, b'\n');
            assert_eq!(previous_count, 1);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            let previous_len = view.len;

            assert_eq!(
                mmap_engine_scan_chunks_pattern(h, 4, std::ptr::null(), 0),
                0
            );
            assert!(
                !mmap_engine_last_error().is_null(),
                "zero-length delimiter must set an error"
            );
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, previous_len);

            assert_eq!(
                mmap_engine_scan_chunks_pattern(h, 4, std::ptr::null(), 2),
                0
            );
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, previous_len);

            let non_null = *b"\n";
            assert_eq!(
                mmap_engine_scan_chunks_pattern(h, 4, non_null.as_ptr(), 0),
                0
            );
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, previous_len);

            assert_eq!(
                mmap_engine_scan_chunks_pattern(
                    h,
                    4,
                    std::ptr::NonNull::<u8>::dangling().as_ptr(),
                    isize::MAX as usize + 1,
                ),
                0
            );
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, previous_len);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_pattern_edge_matrix_and_page_boundary() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_pattern_edges");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cases: &[PatternCase<'_>] = &[
            ("short", &b"hi"[..], &b"\r\n\r\n"[..], 1024, 1),
            ("exact", &b"AB"[..], &b"AB"[..], 1, 1),
            ("begin", &b"||tail"[..], &b"||"[..], 1, 1),
            ("eof", &b"tail||"[..], &b"||"[..], 1, 1),
            ("missing", &b"no marker"[..], &b"||"[..], 4, 1),
            ("consecutive", &b"||||"[..], &b"||"[..], 1, 2),
            ("aba", &b"xABABAy"[..], &b"ABA"[..], 1, 2),
            ("aaaa", &b"AAAAAA"[..], &b"AAAA"[..], 1, 2),
        ];

        for &(name, data, delimiter, chunk_size, expected_count) in cases {
            let file_path = dir.join(format!("{name}.dat"));
            std::fs::write(&file_path, data).unwrap();
            let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
            unsafe {
                let h = mmap_engine_open(c_path.as_ptr());
                assert!(!h.is_null(), "open failed for {name}");
                let count = mmap_engine_scan_chunks_pattern(
                    h,
                    chunk_size,
                    delimiter.as_ptr(),
                    delimiter.len(),
                );
                assert_eq!(count, expected_count, "wrong count for {name}");
                mmap_engine_free(h);
            }
        }

        let mut page_data = vec![b'x'; 4095];
        page_data.extend_from_slice(b"XYZtail");
        let page_path = dir.join("page_boundary.dat");
        std::fs::write(&page_path, &page_data).unwrap();
        let page_c_path = std::ffi::CString::new(page_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(page_c_path.as_ptr());
            assert!(!h.is_null());
            let delimiter = b"XYZ";
            let count =
                mmap_engine_scan_chunks_pattern(h, 4090, delimiter.as_ptr(), delimiter.len());
            assert_eq!(count, 2);
            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, 4098, "pattern crossing page boundary not found");
            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_pattern_mode_switching() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_pattern_modes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        let data = b"aa\r\nbb\r\ncc\r\ndd\r\n";
        std::fs::write(&file_path, data).unwrap();
        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();

        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());
            let pattern = b"\r\n";
            assert!(mmap_engine_scan_chunks_pattern(h, 4, pattern.as_ptr(), pattern.len()) > 0);
            assert!(mmap_engine_scan_chunks_ex(h, 4, b'\n') > 0);
            assert!(mmap_engine_scan_fixed(h, 4) > 0);
            assert!(mmap_engine_partition_records(h, 2, b'\n') > 0);
            assert!(mmap_engine_scan_chunks_pattern(h, 4, pattern.as_ptr(), pattern.len()) > 0);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(
                std::slice::from_raw_parts(view.data, view.len),
                b"aa\r\nbb\r\n"
            );
            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_chunks_pattern_differential_randomized() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_pattern_random");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = 0x5eed_u64;

        for case in 0..256usize {
            let next = |state: &mut u64| {
                *state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (*state >> 32) as u8
            };
            let data_len = (next(&mut state) as usize) % 192;
            let mut data = vec![0u8; data_len];
            for byte in &mut data {
                *byte = next(&mut state);
            }
            let delimiter_len = 1 + (next(&mut state) as usize % 8);
            let mut delimiter = vec![0u8; delimiter_len];
            for byte in &mut delimiter {
                *byte = next(&mut state);
            }
            let chunk_size = next(&mut state) as usize % 32;
            let expected = naive_pattern_boundaries(&data, chunk_size, &delimiter);
            let rust = scanner::find_chunk_boundaries_pattern(&data, chunk_size, &delimiter);
            assert_eq!(rust, expected, "Rust oracle mismatch in case {case}");

            let file_path = dir.join(format!("case_{case}.dat"));
            std::fs::write(&file_path, &data).unwrap();
            let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
            unsafe {
                let h = mmap_engine_open(c_path.as_ptr());
                assert!(!h.is_null(), "open failed in case {case}");
                let count = mmap_engine_scan_chunks_pattern(
                    h,
                    chunk_size,
                    delimiter.as_ptr(),
                    delimiter.len(),
                );
                assert_eq!(count, expected.len(), "count mismatch in case {case}");
                let mut view = CChunkView {
                    data: std::ptr::null(),
                    len: 0,
                };
                for (index, &(start, end)) in expected.iter().enumerate() {
                    assert_eq!(mmap_engine_get_chunk(h, index, &mut view), 0);
                    assert_eq!(view.len, end - start, "length mismatch in case {case}");
                    assert_eq!(
                        std::slice::from_raw_parts(view.data, view.len),
                        &data[start..end],
                        "content mismatch in case {case}"
                    );
                }
                mmap_engine_free(h);
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_oob_after_iteration_newline() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_oob_after_iter");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"first\nsecond\nthird\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_chunks_ex(h, 1, b'\n');
            assert_eq!(count, 3);

            // Verify OOB returns -1 with correct error
            let mut v = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let ret = mmap_engine_get_chunk(h, 999, &mut v);
            assert_eq!(ret, -1, "OOB must return -1");
            let err = mmap_engine_last_error();
            assert!(!err.is_null(), "must have error after OOB");
            let msg = std::ffi::CStr::from_ptr(err).to_string_lossy();
            assert!(
                msg.contains("out of bounds"),
                "expected bounds error, got: {msg}"
            );

            // Verify NULL out_chunk
            let ret2 = mmap_engine_get_chunk(h, 0, std::ptr::null_mut());
            assert_eq!(ret2, -1, "NULL out must return -1");
            let err2 = mmap_engine_last_error();
            assert!(!err2.is_null());
            let msg2 = std::ffi::CStr::from_ptr(err2).to_string_lossy();
            assert!(msg2.contains("out_chunk is null"), "got: {msg2}");

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Fixed-size chunking tests ──────────────────────────────────────

    #[test]
    fn test_scan_fixed_exact_split() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_exact");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"AAAABBBBCCCCDDDDEEEEFFFF").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 4);
            assert_eq!(count, 6);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            for i in 0..count {
                let ret = mmap_engine_get_chunk(h, i, &mut view);
                assert_eq!(ret, 0, "get_chunk failed at {i}");
                assert_eq!(view.len, 4, "chunk {i} must be exactly 4 bytes");
            }

            let mut total = 0usize;
            for i in 0..count {
                mmap_engine_get_chunk(h, i, &mut view);
                total += view.len;
            }
            assert_eq!(total, 24);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_short_last_chunk() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_short");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"XXXXXXXXX").unwrap(); // 9 bytes

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 4);
            assert_eq!(count, 3);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 4);
            mmap_engine_get_chunk(h, 1, &mut view);
            assert_eq!(view.len, 4);
            mmap_engine_get_chunk(h, 2, &mut view);
            assert_eq!(view.len, 1);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_size_larger_than_file() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_large");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"tiny").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 1024);
            assert_eq!(count, 1);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 4);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_chunk_size_zero() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_zero_cs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"abcdef").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 0);
            assert_eq!(count, 6, "chunk_size=0 clamps to 1 → 6 chunks");

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            for i in 0..count {
                let ret = mmap_engine_get_chunk(h, i, &mut view);
                assert_eq!(ret, 0);
                assert_eq!(view.len, 1);
            }

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_empty_file() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("empty.dat");
        std::fs::File::create(&file_path).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 256);
            assert_eq!(count, 0);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_null_handle() {
        unsafe {
            assert_eq!(mmap_engine_scan_fixed(std::ptr::null_mut(), 1024), 0);
            let err = mmap_engine_last_error();
            assert!(!err.is_null());
        }
    }

    #[test]
    fn test_mode_switching_fixed_and_delimited() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_mode_switch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"aaa\nbbb\nccc\nddd\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };

            // Delimited scan: chunk_size=4, delimiter=\n
            // File: "aaa\nbbb\nccc\nddd\n" = 16 bytes
            // Scanner steps to 4, finds \n at relative +3 → chunk (0, 8) = "aaa\nbbb\n"
            let dc = mmap_engine_scan_chunks_ex(h, 4, b'\n');
            assert!(dc > 0);
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 8);

            // Switch to fixed: 4-byte chunks
            let fc = mmap_engine_scan_fixed(h, 4);
            assert!(fc > 0);
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 4);

            // Switch back to delimited
            let dc2 = mmap_engine_scan_chunks_ex(h, 4, b'\n');
            assert_eq!(dc2, dc);
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 8);

            // Fixed with different size
            let fc2 = mmap_engine_scan_fixed(h, 8);
            assert!(fc2 > 0);
            assert_ne!(fc2, fc);
            mmap_engine_get_chunk(h, 0, &mut view);
            assert_eq!(view.len, 8);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_coverage_equals_file_len() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_coverage");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        let content = b"0123456789ABCDEF";
        std::fs::write(&file_path, content).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 3);
            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let mut total = 0usize;
            let mut pos = 0usize;
            for i in 0..count {
                let ret = mmap_engine_get_chunk(h, i, &mut view);
                assert_eq!(ret, 0);
                let slice = std::slice::from_raw_parts(view.data, view.len);
                assert_eq!(slice, &content[pos..pos + view.len]);
                total += view.len;
                pos += view.len;
            }
            assert_eq!(total, content.len());

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_fixed_oob() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_fixed_oob");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.dat");
        std::fs::write(&file_path, b"hello").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let count = mmap_engine_scan_fixed(h, 2);
            assert_eq!(count, 3);

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };
            let ret = mmap_engine_get_chunk(h, 3, &mut view);
            assert_eq!(ret, -1);

            mmap_engine_free(h);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Record partition C ABI tests ──────────────────────────────────

    #[test]
    fn test_partition_records_null_handle() {
        unsafe {
            assert_eq!(
                mmap_engine_partition_records(std::ptr::null_mut(), 4, b'\n'),
                0
            );
        }
    }

    #[test]
    fn test_partition_records_zero_partitions() {
        let dir = std::env::temp_dir().join("mmap_chunker_test_partition_zero");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"a\nb\nc\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());
            assert_eq!(mmap_engine_partition_records(h, 0, b'\n'), 0);
            mmap_engine_free(h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_partition_records_basic() {
        let dir = std::env::temp_dir().join("mmap_chunker_test_partition_basic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        let content = b"record1\nrecord2\nrecord3\nrecord4\n";
        std::fs::write(&file_path, content).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };

            // 2 partitions of 4 records
            let count = mmap_engine_partition_records(h, 2, b'\n');
            assert!(count == 2);
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert!(view.len > 0);
            assert_eq!(mmap_engine_get_chunk(h, 1, &mut view), 0);
            assert!(view.len > 0);
            // OOB
            assert_eq!(mmap_engine_get_chunk(h, 2, &mut view), -1);

            // Verify complete coverage
            let mut total = 0usize;
            for i in 0..count {
                assert_eq!(mmap_engine_get_chunk(h, i, &mut view), 0);
                total += view.len;
            }
            assert_eq!(total, content.len());

            mmap_engine_free(h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_partition_records_no_delimiter() {
        let dir = std::env::temp_dir().join("mmap_chunker_test_partition_nodelim");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        let content = b"no_newlines_at_all";
        std::fs::write(&file_path, content).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };

            // No delimiter -> entire file is one partition
            let count = mmap_engine_partition_records(h, 8, b'\n');
            assert_eq!(count, 1);
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, content.len());
            assert_eq!(mmap_engine_get_chunk(h, 1, &mut view), -1);

            mmap_engine_free(h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_partition_records_empty_file() {
        let dir = std::env::temp_dir().join("mmap_chunker_test_partition_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());
            assert_eq!(mmap_engine_partition_records(h, 4, b'\n'), 0);
            mmap_engine_free(h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_partition_mode_switching_delimited_fixed_partitioned() {
        let dir = std::env::temp_dir().join("mmap_chunker_test_partition_mode_switch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        // 10 records: "line0\n" through "line9\n" = 60 bytes
        let mut content = Vec::new();
        for i in 0..10 {
            content.extend_from_slice(format!("line{i}\n").as_bytes());
        }
        std::fs::write(&file_path, &content).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };

            // 1. Delimited scan
            let dc = mmap_engine_scan_chunks_ex(h, 12, b'\n');
            assert!(dc > 0);

            // 2. Switch to fixed
            let fc = mmap_engine_scan_fixed(h, 10);
            assert!(fc > 0);
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, 10);

            // 3. Switch to partitioned
            let pc = mmap_engine_partition_records(h, 3, b'\n');
            assert_eq!(pc, 3);
            // Verify total coverage
            let mut total = 0usize;
            for i in 0..pc {
                assert_eq!(mmap_engine_get_chunk(h, i, &mut view), 0);
                total += view.len;
            }
            assert_eq!(total, content.len());

            // 4. Switch back to fixed
            mmap_engine_scan_fixed(h, 20);
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert_eq!(view.len, 20);

            // 5. Switch back to delimited
            mmap_engine_scan_chunks_ex(h, 12, b'\n');
            assert_eq!(mmap_engine_get_chunk(h, 0, &mut view), 0);
            assert!(view.len > 0);

            // 6. Switch back to partitioned with different N
            let pc2 = mmap_engine_partition_records(h, 5, b'\n');
            assert_eq!(pc2, 5);

            mmap_engine_free(h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_partition_records_giant_record() {
        let dir = std::env::temp_dir().join("mmap_chunker_test_partition_giant");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        let mut content = b"small1\n".to_vec();
        content.extend(vec![b'x'; 5000]); // giant, no delimiter
        content.extend_from_slice(b"small2\n");
        std::fs::write(&file_path, &content).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        unsafe {
            let h = mmap_engine_open(c_path.as_ptr());
            assert!(!h.is_null());

            let mut view = CChunkView {
                data: std::ptr::null(),
                len: 0,
            };

            // Request many partitions; should collapse due to giant record
            let count = mmap_engine_partition_records(h, 32, b'\n');
            assert!(count < 32, "giant record should collapse boundaries");
            assert!(count > 0, "should have at least one partition");

            // Verify complete coverage
            let mut total = 0usize;
            for i in 0..count {
                assert_eq!(mmap_engine_get_chunk(h, i, &mut view), 0);
                total += view.len;
            }
            assert_eq!(total, content.len());

            // Verify giant record is not split
            for i in 0..count.saturating_sub(1) {
                assert_eq!(mmap_engine_get_chunk(h, i, &mut view), 0);
                if view.len > 0 {
                    let last_byte = *view.data.add(view.len - 1);
                    assert_eq!(last_byte, b'\n', "non-final partition must end after \\n");
                }
            }

            mmap_engine_free(h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
