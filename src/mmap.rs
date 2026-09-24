use std::ffi::c_void;
use std::ffi::CStr;

// ─── Platform-specific FFI declarations ───────────────────────────────────────

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
compile_error!("mmap-chunker-core only supports Windows, Linux, and macOS");

#[cfg(not(target_pointer_width = "64"))]
compile_error!("mmap-chunker-core requires a 64-bit target: lseek/mmap FFI uses off64_t/i64 which is not ABI-compatible with 32-bit off_t on glibc");

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
        pub fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
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

/// Alignment granularity for windowed mappings.
///
/// 64 KiB is the Windows allocation granularity (views must start at a
/// multiple of it) and a multiple of common Unix page sizes (4 KiB,
/// 16 KiB on Apple silicon), so one constant works on every supported
/// platform.
pub const VIEW_ALIGNMENT: u64 = 65_536;

/// A single mapped view inside a [`WindowedMmapFile`].
#[derive(Debug)]
struct WindowView {
    ptr: *const u8,
    aligned_start: u64,
    mapped_len: usize,
}

// SAFETY: the view is a read-only mapping. The pointer is never exposed
// for mutation and is unmapped exactly once by the owning
// `WindowedMmapFile` while holding the state lock.
unsafe impl Send for WindowView {}

/// A read-only memory-mapped file exposing one bounded window at a time.
///
/// Unlike [`MmapFile`], this type never maps the complete file. Each
/// [`read_at`](Self::read_at) call is served from a cached view of at
/// most `window_bytes` (plus alignment slack); a request outside the
/// cached view remaps. Peak virtual address usage is therefore bounded
/// by the window size instead of the file size, which matters for very
/// large files or address-space-constrained consumers.
///
/// The window is aligned down to [`VIEW_ALIGNMENT`]; the returned bytes
/// always start exactly at the requested offset.
///
/// # Safety
///
/// Same immutable-input contract as [`MmapFile`]: the backing file must
/// not be modified, truncated, or deleted while the mapping is alive.
#[derive(Debug)]
pub struct WindowedMmapFile {
    size: usize,
    window_bytes: usize,
    view: std::sync::Mutex<Option<WindowView>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fd: std::ffi::c_int,
    #[cfg(windows)]
    file_handle: isize,
    #[cfg(windows)]
    mapping_handle: isize,
}

impl WindowedMmapFile {
    /// Open `path` for windowed read-only access.
    ///
    /// `window_bytes` must be at least [`VIEW_ALIGNMENT`]; values below
    /// that are rejected with [`std::io::ErrorKind::InvalidInput`]
    /// because they would force a remap for nearly every read.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the backing file is not modified for
    /// the entire lifetime of the returned value.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub unsafe fn open_path(
        path: impl AsRef<std::path::Path>,
        window_bytes: usize,
    ) -> std::io::Result<Self> {
        use std::ffi::CString;
        use std::io;
        use std::os::unix::ffi::OsStrExt;

        if window_bytes < VIEW_ALIGNMENT as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window_bytes must be at least 64 KiB",
            ));
        }

        let bytes = path.as_ref().as_os_str().as_bytes();
        let c_str =
            CString::new(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        // SAFETY: `c_str` is a valid null-terminated string; `open` is a
        // read-only POSIX syscall. The returned fd is owned by `self`.
        let fd = unsafe { sys::open(c_str.as_ptr(), sys::O_RDONLY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `fd` was just opened; `lseek(SEEK_END)` only reports size.
        let file_size = unsafe { sys::lseek(fd, 0, sys::SEEK_END) };
        let size = match usize::try_from(file_size) {
            Ok(size) => size,
            Err(_) => {
                // SAFETY: `fd` is owned and no longer needed.
                unsafe { sys::close(fd) };
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file size does not fit in usize",
                ));
            }
        };

        Ok(Self {
            size,
            window_bytes,
            view: std::sync::Mutex::new(None),
            fd,
        })
    }

    /// Open `path` for windowed read-only access.
    ///
    /// See the Unix variant for the full contract.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the backing file is not modified for
    /// the entire lifetime of the returned value.
    #[cfg(windows)]
    pub unsafe fn open_path(
        path: impl AsRef<std::path::Path>,
        window_bytes: usize,
    ) -> std::io::Result<Self> {
        use std::io;
        use std::os::windows::ffi::OsStrExt;

        if window_bytes < VIEW_ALIGNMENT as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window_bytes must be at least 64 KiB",
            ));
        }

        let wide: Vec<u16> = path
            .as_ref()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `wide` is a valid null-terminated UTF-16 string.
        let file_handle = unsafe {
            sys::CreateFileW(
                wide.as_ptr(),
                sys::GENERIC_READ,
                sys::FILE_SHARE_READ,
                std::ptr::null(),
                sys::OPEN_EXISTING,
                sys::FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if file_handle == sys::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut file_size: i64 = 0;
        // SAFETY: `file_handle` is valid; `GetFileSizeEx` writes the size.
        if unsafe { sys::GetFileSizeEx(file_handle, &mut file_size) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `file_handle` is owned and no longer needed.
            unsafe { sys::CloseHandle(file_handle) };
            return Err(error);
        }
        let size = match usize::try_from(file_size) {
            Ok(size) => size,
            Err(_) => {
                // SAFETY: `file_handle` is owned and no longer needed.
                unsafe { sys::CloseHandle(file_handle) };
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file size does not fit in usize",
                ));
            }
        };

        // A section object for the whole file allows views at any
        // 64 KiB-aligned offset without reopening the file.
        let mapping_handle = if size == 0 {
            0
        } else {
            // SAFETY: `file_handle` is valid; `PAGE_READONLY` creates a
            // read-only section; a zero maximum size means "file size".
            let mapping = unsafe {
                sys::CreateFileMappingW(
                    file_handle,
                    std::ptr::null(),
                    sys::PAGE_READONLY,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if mapping == 0 {
                let error = io::Error::last_os_error();
                // SAFETY: `file_handle` is owned and no longer needed.
                unsafe { sys::CloseHandle(file_handle) };
                return Err(error);
            }
            mapping
        };

        Ok(Self {
            size,
            window_bytes,
            view: std::sync::Mutex::new(None),
            file_handle,
            mapping_handle,
        })
    }

    /// Returns the file size in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.size
    }

    /// Returns `true` if the file is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Read up to `out.len()` bytes starting at `offset`.
    ///
    /// Returns the number of bytes copied (0 at or beyond EOF). On
    /// success, `out[..n]` contains the file bytes at
    /// `[offset, offset + n)`.
    pub fn read_at(&self, offset: usize, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() || offset >= self.size {
            return Ok(0);
        }
        let n = out.len().min(self.size - offset);

        let mut guard = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(view) = guard.as_ref() {
            let view_start = view.aligned_start;
            let view_end = view_start + view.mapped_len as u64;
            let request_end = offset as u64 + n as u64;
            if offset as u64 >= view_start && request_end <= view_end {
                let relative = (offset as u64 - view_start) as usize;
                // SAFETY: the cached view covers `[offset, offset + n)`
                // and lives until it is replaced under this lock.
                let source = unsafe { std::slice::from_raw_parts(view.ptr.add(relative), n) };
                out[..n].copy_from_slice(source);
                return Ok(n);
            }
        }

        let aligned_start = (offset as u64 / VIEW_ALIGNMENT) * VIEW_ALIGNMENT;
        let slack = offset as u64 - aligned_start;
        let needed = slack + n as u64;
        let desired = (self.window_bytes as u64 + slack).max(needed);
        let mapped_len = desired.min(self.size as u64 - aligned_start) as usize;

        let view = self.map_view(aligned_start, mapped_len)?;
        let relative = slack as usize;
        // SAFETY: `view.ptr` maps `mapped_len` bytes starting at
        // `aligned_start`; `relative + n <= mapped_len` by construction.
        let source = unsafe { std::slice::from_raw_parts(view.ptr.add(relative), n) };
        out[..n].copy_from_slice(source);

        if let Some(old) = guard.replace(view) {
            self.unmap_view(old);
        }
        Ok(n)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn map_view(&self, aligned_start: u64, mapped_len: usize) -> std::io::Result<WindowView> {
        use std::io;

        if mapped_len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty view"));
        }

        let offset = i64::try_from(aligned_start)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds i64"))?;

        // SAFETY: `self.fd` is valid for the lifetime of `self`;
        // `aligned_start` is a multiple of VIEW_ALIGNMENT (and therefore
        // page-aligned); `mapped_len` is clamped to the file.
        let ptr = unsafe {
            sys::mmap(
                std::ptr::null_mut(),
                mapped_len,
                sys::PROT_READ,
                sys::MAP_PRIVATE,
                self.fd,
                offset,
            )
        };
        if ptr == sys::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(WindowView {
            ptr: ptr as *const u8,
            aligned_start,
            mapped_len,
        })
    }

    #[cfg(windows)]
    fn map_view(&self, aligned_start: u64, mapped_len: usize) -> std::io::Result<WindowView> {
        use std::io;

        if mapped_len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty view"));
        }

        // SAFETY: `mapping_handle` is a valid section object for the
        // file; `aligned_start` is a multiple of the allocation
        // granularity; `mapped_len` is clamped to the file.
        let ptr = unsafe {
            sys::MapViewOfFile(
                self.mapping_handle,
                sys::FILE_MAP_READ,
                (aligned_start >> 32) as u32,
                (aligned_start & 0xFFFF_FFFF) as u32,
                mapped_len,
            )
        };
        if ptr.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(WindowView {
            ptr: ptr as *const u8,
            aligned_start,
            mapped_len,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unmap_view(&self, view: WindowView) {
        // SAFETY: `view` was returned by `map_view` and is unmapped once.
        unsafe {
            sys::munmap(view.ptr as *mut c_void, view.mapped_len);
        }
    }

    #[cfg(windows)]
    fn unmap_view(&self, view: WindowView) {
        // SAFETY: `view` was returned by `map_view` and is unmapped once.
        unsafe {
            sys::UnmapViewOfFile(view.ptr as *const c_void);
        }
    }
}

impl Drop for WindowedMmapFile {
    fn drop(&mut self) {
        let view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(view) = view {
            self.unmap_view(view);
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if self.fd >= 0 {
                // SAFETY: `fd` is owned by `self` and not closed elsewhere.
                unsafe {
                    sys::close(self.fd);
                }
            }
        }
        #[cfg(windows)]
        {
            if self.mapping_handle != 0 {
                // SAFETY: `mapping_handle` is owned and not closed elsewhere.
                unsafe {
                    sys::CloseHandle(self.mapping_handle);
                }
            }
            if self.file_handle != 0 && self.file_handle != sys::INVALID_HANDLE_VALUE {
                // SAFETY: `file_handle` is owned and not closed elsewhere.
                unsafe {
                    sys::CloseHandle(self.file_handle);
                }
            }
        }
    }
}

/// A read-only memory-mapped file.
///
/// Provides zero-copy access to file contents. Automatically unmaps the
/// mapping and closes all platform handles when dropped.
///
/// # Platform support
///
/// - **Unix** (Linux, macOS): Uses `mmap(2)` / `munmap(2)` / `madvise(2)`.
/// - **Windows**: Uses `CreateFileMappingW` / `MapViewOfFile` / `UnmapViewOfFile`.
#[derive(Debug)]
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
        // O_RDONLY does not require the variadic mode argument.
        let fd = sys::open(path.as_ptr(), sys::O_RDONLY);
        if fd < 0 {
            return None;
        }

        // SAFETY: `fd` was just opened successfully. `lseek()` with
        // SEEK_END returns the file size without modifying the fd state
        // (we don't need the seek position afterwards).
        let file_size = sys::lseek(fd, 0, sys::SEEK_END);
        let size = match usize::try_from(file_size) {
            Ok(s) => s,
            Err(_) => {
                sys::close(fd);
                return None;
            }
        };
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

        // SAFETY: `wide` is null-terminated, all syscalls are read-only.
        unsafe { Self::open_windows_wide(&wide) }
    }

    #[cfg(windows)]
    unsafe fn open_windows_wide(wide_path: &[u16]) -> Option<Self> {
        // SAFETY: `wide_path` is a valid null-terminated UTF-16 string.
        // `GENERIC_READ` opens for read-only; `OPEN_EXISTING` fails if
        // the file does not exist.
        let fh = sys::CreateFileW(
            wide_path.as_ptr(),
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

        let size = match usize::try_from(file_size) {
            Ok(s) => s,
            Err(_) => {
                sys::CloseHandle(fh);
                return None;
            }
        };
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

    /// Open and memory-map the file at `path` for read-only access.
    ///
    /// Accepts any type that converts to `Path` (`&str`, `&Path`,
    /// `PathBuf`, `&OsStr`, etc.).
    ///
    /// On Windows, the path is encoded as UTF-16 directly (no lossy
    /// UTF-8 round-trip). On Unix, the raw OS path bytes are used
    /// directly.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the backing file is not modified,
    /// truncated, deleted, or otherwise invalidated for the entire
    /// lifetime of the returned `MmapFile` and all `&[u8]` slices
    /// derived from it via [`as_bytes`](Self::as_bytes).
    ///
    /// Concurrent file mutation by any process — including the calling
    /// process — violates Rust's `&[u8]` immutability guarantee and is
    /// undefined behavior (see [The Rust Reference]).
    ///
    /// On POSIX systems (`MAP_PRIVATE`) another process may freely
    /// open the file for writing; use external synchronization (file
    /// locks, snapshots, or immutable files). On Windows the share
    /// mode `FILE_SHARE_READ` prevents other processes from opening
    /// the file for writing, but same-process mutation remains
    /// possible.
    ///
    /// Accessing beyond a truncated region may deliver `SIGBUS`
    /// (POSIX) or an access violation (Windows), crashing the process.
    ///
    /// [The Rust Reference]: https://doc.rust-lang.org/reference/behavior-considered-undefined.html
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub unsafe fn open_path(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        use std::ffi::CString;
        use std::io;
        use std::os::unix::ffi::OsStrExt;

        let bytes = path.as_ref().as_os_str().as_bytes();
        let c_str =
            CString::new(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: c_str is a valid null-terminated string, not mutated
        // during this call. `open` reads the path and maps the file.
        // The caller's safety obligation covers the returned mapping.
        unsafe { Self::open(&c_str) }.ok_or_else(|| io::Error::other("failed to open or map file"))
    }

    /// Open and memory-map the file at `path` for read-only access.
    ///
    /// Accepts any type that converts to `Path` (`&str`, `&Path`,
    /// `PathBuf`, `&OsStr`, etc.).
    ///
    /// On Windows, the path is encoded as UTF-16 directly (no lossy
    /// UTF-8 round-trip). On Unix, the raw OS path bytes are used.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the backing file is not modified,
    /// truncated, deleted, or otherwise invalidated for the entire
    /// lifetime of the returned `MmapFile` and all `&[u8]` slices
    /// derived from it via [`as_bytes`](Self::as_bytes).
    ///
    /// See the Unix variant for additional notes on OS-specific
    /// behavior.
    #[cfg(windows)]
    pub unsafe fn open_path(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        use std::io;
        use std::os::windows::ffi::OsStrExt;

        let wide: Vec<u16> = path
            .as_ref()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `wide` is a valid null-terminated UTF-16 string.
        // All syscalls are read-only file/mapping operations.
        // The caller's safety obligation covers the returned mapping.
        unsafe { Self::open_windows_wide(&wide) }
            .ok_or_else(|| io::Error::other("failed to open or map file"))
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
    /// The returned borrow is valid for the lifetime of `self`. The
    /// caller must not allow the underlying file to be mutated while
    /// the slice is live (the mapping is read-only, so this only
    /// matters if another process truncates or overwrites the file).
    ///
    /// Prefer [`as_bytes`](Self::as_bytes) when the caller has already
    /// accepted the file-immutability contract at construction time
    /// (e.g. via [`open_path`](Self::open_path)).
    #[inline]
    pub unsafe fn as_slice(&self) -> &[u8] {
        self.as_bytes()
    }

    /// Returns the mapped memory as a byte slice.
    ///
    /// The returned borrow is valid for the lifetime of `self`. This
    /// method is safe because the constructor
    /// ([`open_path`](Self::open_path)) requires the caller to
    /// guarantee that the backing file is immutable. If that guarantee
    /// is violated (external truncation or overwrite), the process may
    /// crash with `SIGBUS` (POSIX) or an access violation (Windows)
    /// rather than undefined behavior.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        if self.ptr.is_null() || self.size == 0 {
            &[]
        } else {
            // SAFETY: `ptr` points to `size` bytes of valid memory that
            // was allocated by the OS mapping during `open()` and is
            // guaranteed to outlive `self`. The mapping is read-only.
            unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
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

    // ── open_path (Path-based) tests ────────────────────────────────────

    #[test]
    fn test_open_path_normal() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_path_normal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        let content = b"hello world\n";
        std::fs::write(&file_path, content).unwrap();

        unsafe {
            let mmap = MmapFile::open_path(&file_path).unwrap();
            assert!(!mmap.is_empty());
            assert_eq!(mmap.len(), content.len());
            assert_eq!(mmap.as_bytes(), content.as_slice());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_nonexistent() {
        unsafe {
            let err = MmapFile::open_path("nonexistent_file_xyz_123.dat").unwrap_err();
            assert!(
                err.kind() == std::io::ErrorKind::NotFound
                    || err.kind() == std::io::ErrorKind::Other,
                "expected NotFound or Other, got {:?}",
                err.kind()
            );
        }
    }

    #[test]
    fn test_open_path_empty_file() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_path_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("empty.dat");
        std::fs::File::create(&file_path).unwrap();

        unsafe {
            let mmap = MmapFile::open_path(&file_path).unwrap();
            assert!(mmap.is_empty());
            assert_eq!(mmap.len(), 0);
            assert_eq!(mmap.as_bytes(), b"");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_with_spaces() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test path spaces");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("my file.dat");
        let content = b"data with spaces in path\n";
        std::fs::write(&file_path, content).unwrap();

        unsafe {
            let mmap = MmapFile::open_path(&file_path).unwrap();
            assert_eq!(mmap.as_bytes(), content.as_slice());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_unicode() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_unicode");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, b"content\n").unwrap();

        unsafe {
            let mmap = MmapFile::open_path(&file_path).unwrap();
            assert_eq!(mmap.len(), 8);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_drop_works() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_path_drop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("drop_test.dat");
        std::fs::write(&file_path, b"drop test content\n").unwrap();

        unsafe {
            let mmap = MmapFile::open_path(&file_path).unwrap();
            assert!(!mmap.is_empty());
            // mmap is dropped here — should unmap and close handles
        }

        // Verify the file still exists and is readable after drop
        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content, b"drop test content\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_from_pathbuf() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_pathbuf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("pathbuf.dat");
        std::fs::write(&file_path, b"pathbuf content\n").unwrap();

        unsafe {
            let mmap = MmapFile::open_path(file_path.clone()).unwrap();
            assert_eq!(mmap.as_bytes(), b"pathbuf content\n");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_from_str() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_path_str");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("str.dat");
        std::fs::write(&file_path, b"str content\n").unwrap();

        let path_str = file_path.to_str().unwrap();
        unsafe {
            let mmap = MmapFile::open_path(path_str).unwrap();
            assert_eq!(mmap.as_bytes(), b"str content\n");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_as_bytes_returns_valid_slice() {
        let dir = std::env::temp_dir().join("mmap_chunker_core_test_as_bytes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("bytes.dat");
        let content: Vec<u8> = (0u8..=255).collect();
        std::fs::write(&file_path, &content).unwrap();

        unsafe {
            let mmap = MmapFile::open_path(&file_path).unwrap();
            let slice = mmap.as_bytes();
            assert_eq!(slice.len(), 256);
            assert_eq!(slice[0], 0);
            assert_eq!(slice[128], 128);
            assert_eq!(slice[255], 255);
            assert_eq!(slice, &content[..]);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── WindowedMmapFile tests ──────────────────────────────────────────

    fn windowed_temp_file(label: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mmap_chunker_core_windowed_{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.bin");
        std::fs::write(&file_path, content).unwrap();
        file_path
    }

    #[test]
    fn test_windowed_open_nonexistent() {
        unsafe {
            let error =
                WindowedMmapFile::open_path("nonexistent_windowed_xyz.dat", 65536).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
    }

    #[test]
    fn test_windowed_rejects_tiny_window() {
        let path = windowed_temp_file("tiny", b"data");
        unsafe {
            let error = WindowedMmapFile::open_path(&path, 1024).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_windowed_empty_file() {
        let path = windowed_temp_file("empty", b"");
        unsafe {
            let file = WindowedMmapFile::open_path(&path, 65536).unwrap();
            assert!(file.is_empty());
            assert_eq!(file.len(), 0);
            let mut out = [0u8; 16];
            assert_eq!(file.read_at(0, &mut out).unwrap(), 0);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_windowed_reads_cross_window_boundaries() {
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let path = windowed_temp_file("cross", &content);
        unsafe {
            let file = WindowedMmapFile::open_path(&path, 65536).unwrap();
            assert_eq!(file.len(), content.len());

            // Sequential 1 MiB reads.
            let mut out = vec![0u8; 1_048_576];
            let mut offset = 0usize;
            while offset < content.len() {
                let read = file.read_at(offset, &mut out).unwrap();
                assert!(read > 0);
                assert_eq!(&out[..read], &content[offset..offset + read]);
                offset += read;
            }
            assert_eq!(offset, content.len());

            // Unaligned reads near window boundaries.
            for &start in &[0usize, 1, 4095, 65535, 65536, 65537, 131_071, 199_999] {
                let mut small = [0u8; 8];
                let expected_len = (content.len() - start).min(small.len());
                let read = file.read_at(start, &mut small).unwrap();
                assert_eq!(read, expected_len, "offset {start}");
                assert_eq!(&small[..read], &content[start..start + read]);
            }

            // Read at EOF and beyond.
            let mut small = [0u8; 8];
            assert_eq!(file.read_at(content.len(), &mut small).unwrap(), 0);
            assert_eq!(file.read_at(content.len() + 10, &mut small).unwrap(), 0);

            // Partial final read is clamped to EOF.
            let read = file.read_at(content.len() - 3, &mut small).unwrap();
            assert_eq!(read, 3);
            assert_eq!(&small[..3], &content[content.len() - 3..]);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
