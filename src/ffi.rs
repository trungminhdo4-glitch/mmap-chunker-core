//! C-ABI export layer.
//!
//! Provides an opaque engine handle and C-callable functions for
//! memory-mapping files, scanning chunk boundaries, and retrieving
//! zero-copy chunk views.

use std::ffi::{c_char, c_int, CStr};

use crate::mmap::MmapFile;
use crate::scanner;

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
    _private: [u8; 0],
}

// ─── Internal engine state ────────────────────────────────────────────────────

struct Engine {
    mmap: MmapFile,
    chunks: Vec<(usize, usize)>,
}

// ─── FFI functions ────────────────────────────────────────────────────────────

/// Open and memory-map a file for chunked access.
///
/// `path` must be a null-terminated UTF-8 C string.
///
/// Returns a heap-allocated opaque engine handle on success, or null if the
/// file cannot be opened or mapped.
///
/// # Safety
///
/// `path` must point to a valid null-terminated C string. The returned
/// handle must be freed with `mmap_engine_free` exactly once.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_open(path: *const c_char) -> *mut CEngineHandle {
    let inner = move || {
        if path.is_null() {
            return std::ptr::null_mut();
        }

        // SAFETY: caller guarantees `path` is a valid null-terminated C string.
        let c_str = unsafe { CStr::from_ptr(path) };

        // SAFETY: `c_str` is valid and the callee validates the path.
        match unsafe { MmapFile::open(c_str) } {
            Some(mmap) => {
                mmap.advise_sequential();
                let engine = Box::new(Engine {
                    mmap,
                    chunks: Vec::new(),
                });
                Box::into_raw(engine) as *mut CEngineHandle
            }
            None => std::ptr::null_mut(),
        }
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(ptr) => ptr,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Scan the mapped file for chunk boundaries.
///
/// Chunks are created at approximately `chunk_size_bytes` intervals. Each
/// chunk boundary is placed immediately after a newline (`\n`, 0x0A)
/// found at or after the target offset. The last chunk extends to the
/// end of the file.
///
/// Returns the number of chunks found. Returns 0 if the handle is null or
/// the file is empty.
///
/// # Safety
///
/// `handle` must be a valid pointer returned by `mmap_engine_open` and
/// must not have been freed.
#[no_mangle]
pub unsafe extern "C" fn mmap_engine_scan_chunks(
    handle: *mut CEngineHandle,
    chunk_size_bytes: usize,
) -> usize {
    let inner = move || {
        if handle.is_null() {
            return 0;
        }

        // SAFETY: caller guarantees `handle` is valid and not freed.
        let engine = unsafe { &mut *(handle as *mut Engine) };

        let data = unsafe { engine.mmap.as_slice() };
        if data.is_empty() {
            engine.chunks.clear();
            return 0;
        }

        engine.chunks = scanner::find_chunk_boundaries(data, chunk_size_bytes, b'\n');
        engine.chunks.len()
    };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)).unwrap_or_default()
}

/// Retrieve a chunk view by index.
///
/// Writes the chunk's data pointer and length into `out_chunk`. The
/// pointer points directly into the memory-mapped file (zero-copy) and
/// remains valid until `mmap_engine_free` is called.
///
/// Returns 0 on success, -1 if `handle` or `out_chunk` is null, or if
/// `index` is out of bounds.
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
        if handle.is_null() || out_chunk.is_null() {
            return -1;
        }

        // SAFETY: caller guarantees both pointers are valid.
        let engine = unsafe { &*(handle as *mut Engine) };

        if index >= engine.chunks.len() {
            return -1;
        }

        let (start, end) = engine.chunks[index];

        // SAFETY: `out_chunk` is valid and writable (caller guarantee).
        unsafe {
            (*out_chunk).data = engine.mmap.as_ptr().add(start);
            (*out_chunk).len = end - start;
        }

        0
    };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)).unwrap_or(-1)
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
        assert_eq!(std::mem::size_of::<CEngineHandle>(), 0);
        assert_eq!(
            std::mem::size_of::<*const CEngineHandle>(),
            std::mem::size_of::<usize>()
        );
    }
}
