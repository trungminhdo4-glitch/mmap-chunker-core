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

pub const ABI_VERSION: u32 = 0x0001_0000;

pub const CAP_ZERO_COPY: u32 = 1 << 0;
pub const CAP_CONFIGURABLE_DELIMITER: u32 = 1 << 1;
pub const CAP_ERROR_STRINGS: u32 = 1 << 2;

const MAX_ERROR_LEN: usize = 256;

thread_local! {
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

struct Engine {
    mmap: MmapFile,
    chunks: Vec<(usize, usize)>,
}

// ─── ABI discovery ────────────────────────────────────────────────────────────

/// Return the ABI version as `(major << 16) | minor`.
///
/// Current: `0x0001_0000` (v1.0). Always succeeds, never panics.
#[no_mangle]
pub extern "C" fn mmap_engine_abi_version() -> u32 {
    ABI_VERSION
}

/// Return a bitmask of supported capabilities.
///
/// Consumers call this once at load time to discover which optional
/// features the loaded library provides.
///
/// Current bits (v1.0):
///   - Bit 0: `ZERO_COPY` — chunk views reference mapped memory directly
///   - Bit 1: `CONFIGURABLE_DELIMITER` — `mmap_engine_scan_chunks_ex` available
///   - Bit 2: `ERROR_STRINGS` — `mmap_engine_last_error` returns diagnostic text
#[no_mangle]
pub extern "C" fn mmap_engine_capabilities() -> u32 {
    CAP_ZERO_COPY | CAP_CONFIGURABLE_DELIMITER | CAP_ERROR_STRINGS
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
                    chunks: Vec::new(),
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
            engine.chunks.clear();
            return 0;
        }

        engine.chunks = scanner::find_chunk_boundaries(data, chunk_size_bytes, delimiter);
        engine.chunks.len()
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(count) => count,
        Err(_) => {
            set_error("internal error: panic in mmap_engine_scan_chunks");
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

        if index >= engine.chunks.len() {
            set_error("chunk index out of bounds");
            return -1;
        }

        let (start, end) = engine.chunks[index];

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
}
