use std::ffi::c_void;
use std::ffi::CStr;

// ─── Platform-specific FFI declarations ───────────────────────────────────────

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
compile_error!("mmap-chunker-core only supports Windows, Linux, and macOS");

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod sys {
    use std::ffi::{c_char, c_int, c_void};

    pub const O_RDONLY: c_int = 0;
    pub const PROT_READ: c_int = 1;
    pub const MAP_PRIVATE: c_int = 2;
    pub const MAP_FAILED: *mut c_void = (-1isize) as *mut c_void;
    pub const MADV_SEQUENTIAL: c_int = 2;
    pub const SEEK_END: c_int = 2;

    extern "C" {
        pub fn open(pathname: *const c_char, flags: c_int, mode: c_int) -> c_int;
        pub fn close(fd: c_int) -> c_int;
        pub fn mmap(
            addr: *mut c_void,
            length: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, length: usize) -> c_int;
        pub fn madvise(addr: *mut c_void, length: usize, advice: c_int) -> c_int;
        pub fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64;
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    pub const GENERIC_READ: u32 = 0x8000_0000;
    pub const FILE_SHARE_READ: u32 = 0x0000_0001;
    pub const OPEN_EXISTING: u32 = 3;
    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    pub const INVALID_HANDLE_VALUE: isize = -1;
    pub const PAGE_READONLY: u32 = 0x0000_0002;
    pub const FILE_MAP_READ: u32 = 0x0000_0004;

    extern "system" {
        pub fn CreateFileW(
            lpFileName: *const u16,
            dwDesiredAccess: u32,
            dwShareMode: u32,
            lpSecurityAttributes: *const c_void,
            dwCreationDisposition: u32,
            dwFlagsAndAttributes: u32,
            hTemplateFile: isize,
        ) -> isize;

        pub fn CloseHandle(hObject: isize) -> i32;

        pub fn CreateFileMappingW(
            hFile: isize,
            lpFileMappingAttributes: *const c_void,
            flProtect: u32,
            dwMaximumSizeHigh: u32,
            dwMaximumSizeLow: u32,
            lpName: *const u16,
        ) -> isize;

        pub fn MapViewOfFile(
            hFileMappingObject: isize,
            dwDesiredAccess: u32,
            dwFileOffsetHigh: u32,
            dwFileOffsetLow: u32,
            dwNumberOfBytesToMap: usize,
        ) -> *mut c_void;

        pub fn UnmapViewOfFile(lpBaseAddress: *const c_void) -> i32;

        pub fn GetFileSizeEx(hFile: isize, lpFileSize: *mut i64) -> i32;
    }
}

// ─── MmapFile ─────────────────────────────────────────────────────────────────

/// A read-only memory-mapped file.
///
/// Provides zero-copy access to file contents. Automatically unmaps the
/// mapping and closes all platform handles when dropped.
///
/// # Platform support
///
/// - **Unix** (Linux, macOS): Uses `mmap(2)` / `munmap(2)` / `madvise(2)`.
/// - **Windows**: Uses `CreateFileMappingW` / `MapViewOfFile` / `UnmapViewOfFile`.
pub struct MmapFile {
    ptr: *const u8,
    size: usize,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fd: std::ffi::c_int,
    #[cfg(windows)]
    file_handle: isize,
    #[cfg(windows)]
    mapping_handle: isize,
}

impl MmapFile {
    /// Open and memory-map the file at `path` for read-only access.
    ///
    /// `path` must be a valid null-terminated C string (UTF-8 on all
    /// platforms; on Windows it is converted to UTF-16 internally).
    ///
    /// Returns `None` if the file cannot be opened, its size cannot be
    /// determined, or the mapping fails.
    ///
    /// # Safety
    ///
    /// `path` must point to a valid null-terminated C string and must not
    /// be mutated during this call.
    pub unsafe fn open(path: &CStr) -> Option<Self> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Self::open_unix(path)
        }
        #[cfg(windows)]
        {
            Self::open_windows(path)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unsafe fn open_unix(path: &CStr) -> Option<Self> {
        // SAFETY: `path.as_ptr()` is a valid null-terminated string;
        // `open()` is a POSIX syscall that only reads from the path.
        let fd = sys::open(path.as_ptr(), sys::O_RDONLY, 0);
        if fd < 0 {
            return None;
        }

        // SAFETY: `fd` was just opened successfully. `lseek()` with
        // SEEK_END returns the file size without modifying the fd state
        // (we don't need the seek position afterwards).
        let file_size = sys::lseek(fd, 0, sys::SEEK_END);
        if file_size < 0 {
            sys::close(fd);
            return None;
        }

        let size = file_size as usize;
        if size == 0 {
            sys::close(fd);
            return Some(MmapFile {
                ptr: std::ptr::null(),
                size: 0,
                fd: -1,
            });
        }

        // SAFETY: `fd` is valid, `size > 0`, `offset=0` is page-aligned.
        // PROT_READ | MAP_PRIVATE creates a read-only private mapping
        // that does not modify the underlying file.
        let ptr = sys::mmap(
            std::ptr::null_mut(),
            size,
            sys::PROT_READ,
            sys::MAP_PRIVATE,
            fd,
            0,
        );

        if ptr == sys::MAP_FAILED {
            sys::close(fd);
            return None;
        }

        Some(MmapFile {
            ptr: ptr as *const u8,
            size,
            fd,
        })
    }

    #[cfg(windows)]
    unsafe fn open_windows(path: &CStr) -> Option<Self> {
        use std::os::windows::ffi::OsStrExt;

        let path_lossy = path.to_string_lossy();
        let wide: Vec<u16> = std::ffi::OsStr::new(&*path_lossy)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `wide` is a valid null-terminated UTF-16 string.
        // `GENERIC_READ` opens for read-only; `OPEN_EXISTING` fails if
        // the file does not exist.
        let fh = sys::CreateFileW(
            wide.as_ptr(),
            sys::GENERIC_READ,
            sys::FILE_SHARE_READ,
            std::ptr::null(),
            sys::OPEN_EXISTING,
            sys::FILE_ATTRIBUTE_NORMAL,
            0,
        );

        if fh == sys::INVALID_HANDLE_VALUE {
            return None;
        }

        // SAFETY: `fh` is a valid file handle from CreateFileW.
        // `GetFileSizeEx` writes the size into `file_size`.
        let mut file_size: i64 = 0;
        if sys::GetFileSizeEx(fh, &mut file_size) == 0 {
            sys::CloseHandle(fh);
            return None;
        }

        let size = file_size as usize;
        if size == 0 {
            sys::CloseHandle(fh);
            return Some(MmapFile {
                ptr: std::ptr::null(),
                size: 0,
                file_handle: 0,
                mapping_handle: 0,
            });
        }

        // SAFETY: `fh` is valid. `PAGE_READONLY` + `FILE_MAP_READ`
        // creates a read-only mapping. Maximum size 0 means "use file
        // size". `lpName` null means unnamed mapping.
        let mh = sys::CreateFileMappingW(
            fh,
            std::ptr::null(),
            sys::PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        );

        if mh == 0 {
            sys::CloseHandle(fh);
            return None;
        }

        // SAFETY: `mh` is a valid mapping handle. Offset 0 and
        // `dwNumberOfBytesToMap=0` maps the entire file.
        let ptr = sys::MapViewOfFile(mh, sys::FILE_MAP_READ, 0, 0, 0);

        if ptr.is_null() {
            sys::CloseHandle(mh);
            sys::CloseHandle(fh);
            return None;
        }

        Some(MmapFile {
            ptr: ptr as *const u8,
            size,
            file_handle: fh,
            mapping_handle: mh,
        })
    }

    /// Returns a raw pointer to the start of the mapped memory.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Returns the size of the mapping in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.size
    }

    /// Returns `true` if the mapping is empty (zero-length file).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Returns the mapped memory as a byte slice.
    ///
    /// # Safety
    ///
    /// The returned borrow is valid for the lifetime of `self`. The caller
    /// must not mutate the underlying file while the slice is live
    /// (the mapping is read-only, so this only matters if another process
    /// truncates or overwrites the file).
    #[inline]
    pub unsafe fn as_slice(&self) -> &[u8] {
        if self.ptr.is_null() || self.size == 0 {
            &[]
        } else {
            // SAFETY: `ptr` points to `size` bytes of valid memory that
            // was allocated by the OS mapping during `open()` and is
            // guaranteed to outlive `self`.
            std::slice::from_raw_parts(self.ptr, self.size)
        }
    }

    /// Advise the operating system that the mapping will be accessed
    /// sequentially. This is a best-effort performance hint.
    pub fn advise_sequential(&self) {
        if self.ptr.is_null() || self.size == 0 {
            return;
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // SAFETY: `ptr` and `size` are valid from the mapping.
            // `madvise()` with `MADV_SEQUENTIAL` is purely advisory
            // and cannot affect memory safety.
            unsafe {
                sys::madvise(self.ptr as *mut c_void, self.size, sys::MADV_SEQUENTIAL);
            }
        }

        #[cfg(windows)]
        {
            let _ = self;
        }
    }
}

impl Drop for MmapFile {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if !self.ptr.is_null() && self.size > 0 {
                // SAFETY: `ptr` and `size` match the original `mmap()`
                // call from `open()`. The mapping was never aliased by
                // another munmap.
                unsafe {
                    sys::munmap(self.ptr as *mut c_void, self.size);
                }
            }
            if self.fd >= 0 {
                // SAFETY: `fd` is a valid file descriptor from `open()`
                // that has not been closed elsewhere.
                unsafe {
                    sys::close(self.fd);
                }
            }
        }

        #[cfg(windows)]
        {
            if !self.ptr.is_null() {
                // SAFETY: `ptr` is the base address returned by
                // `MapViewOfFile` and has not been unmapped elsewhere.
                unsafe {
                    sys::UnmapViewOfFile(self.ptr as *const c_void);
                }
            }
            if self.mapping_handle != 0 {
                // SAFETY: `mapping_handle` is from `CreateFileMappingW`
                // and has not been closed elsewhere.
                unsafe {
                    sys::CloseHandle(self.mapping_handle);
                }
            }
            if self.file_handle != 0 && self.file_handle != sys::INVALID_HANDLE_VALUE {
                // SAFETY: `file_handle` is from `CreateFileW` and has
                // not been closed elsewhere.
                unsafe {
                    sys::CloseHandle(self.file_handle);
                }
            }
        }
    }
}

// SAFETY: `MmapFile` owns a read-only memory mapping. Both Unix and
// Windows read-only file mappings are process-global resources that are
// safe to share immutably across threads. The inner `*const u8` is never
// exposed for mutation and is stable for the lifetime of the mapping.
unsafe impl Send for MmapFile {}
unsafe impl Sync for MmapFile {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_nonexistent_file() {
        let path = c"/nonexistent/path/does/not/exist";
        unsafe {
            assert!(MmapFile::open(path).is_none());
        }
    }

    #[test]
    fn test_mmap_empty_file() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("empty.dat");
        {
            std::fs::File::create(&file_path).unwrap();
        }

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap().as_bytes()).unwrap();
        unsafe {
            let mmap = MmapFile::open(&c_path).unwrap();
            assert!(mmap.is_empty());
            assert_eq!(mmap.len(), 0);
            assert!(mmap.as_ptr().is_null());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_mmap_small_file() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_small");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("small.dat");
        let content: Vec<u8> = (0u8..=255).collect();
        std::fs::write(&file_path, &content).unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap().as_bytes()).unwrap();
        unsafe {
            let mmap = MmapFile::open(&c_path).unwrap();
            assert!(!mmap.is_empty());
            assert_eq!(mmap.len(), 256);
            let slice = mmap.as_slice();
            assert_eq!(slice, &content[..]);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_advise_sequential_does_not_crash() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_advise");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("advise.dat");
        std::fs::write(&file_path, b"some content\n").unwrap();

        let c_path = std::ffi::CString::new(file_path.to_str().unwrap().as_bytes()).unwrap();
        unsafe {
            let mmap = MmapFile::open(&c_path).unwrap();
            mmap.advise_sequential();
            mmap.advise_sequential();
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
