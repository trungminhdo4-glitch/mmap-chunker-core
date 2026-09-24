//! Byte sources and source-agnostic record-aligned range planning.
//!
//! The slice-based scanner is the reference implementation. This module
//! adds a [`ByteSource`] abstraction so the same planning semantics can
//! run on three backends:
//!
//! | Backend | Mapping | Use case |
//! |---------|---------|----------|
//! | [`MmapSource`] | full file | default; delegates to the slice scanner |
//! | [`WindowedMmapSource`] | one bounded window | very large files, limited address space, NFS |
//! | [`PreadSource`] | none | no mapping at all; positional reads |
//!
//! All three must produce byte-identical partition ranges for the same
//! file and parameters. The windowed and pread path achieves this with a
//! buffered forward search whose overlap handling — and exact-target
//! acceptance — is the streaming equivalent of
//! [`scanner::find_partition_boundaries_pattern`].

use std::fs::File;
use std::io;
use std::path::Path;

use crate::mmap::{MmapFile, WindowedMmapFile, VIEW_ALIGNMENT};
use crate::scanner;

/// Default scan buffer for buffered backends (1 MiB).
pub const DEFAULT_SCAN_BUFFER_BYTES: usize = 1024 * 1024;

/// Default window size for [`WindowedMmapSource`] (64 MiB).
pub const DEFAULT_WINDOW_BYTES: usize = 64 * 1024 * 1024;

/// Minimum accepted window size (one [`VIEW_ALIGNMENT`] unit).
pub const MIN_WINDOW_BYTES: usize = VIEW_ALIGNMENT as usize;

#[cfg(windows)]
const WINDOWS_FILE_SHARE_READ: u32 = 0x0000_0001;

/// A random-access, read-only byte source.
///
/// Implementations must be usable from multiple threads
/// (`Send + Sync`) and must return the exact file bytes for a given
/// offset. Sources that can expose the complete file as one slice
/// should do so via [`as_slice`](ByteSource::as_slice); consumers use it
/// to take the zero-copy reference path.
pub trait ByteSource: Send + Sync {
    /// Total size of the source in bytes.
    fn len(&self) -> usize;

    /// Read up to `out.len()` bytes starting at `offset`.
    ///
    /// Returns the number of bytes copied. A return value of 0 means
    /// the offset is at or beyond the end of the source.
    fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize>;

    /// Return the complete source as a slice when a full mapping (or an
    /// equivalent contiguous view) is available.
    fn as_slice(&self) -> Option<&[u8]>;

    /// Returns `true` if the source is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Full-file mmap source.
///
/// Delegates planning to the slice-based scanner, so this backend is
/// always the reference for the buffered backends.
#[derive(Debug)]
pub struct MmapSource {
    mmap: MmapFile,
}

impl MmapSource {
    /// Open `path` and map the complete file read-only.
    ///
    /// # Safety
    ///
    /// Same immutable-input contract as [`MmapFile::open_path`]: the
    /// file must not be modified while the source is alive.
    pub unsafe fn open_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let mmap = unsafe { MmapFile::open_path(path)? };
        mmap.advise_sequential();
        Ok(Self { mmap })
    }
}

impl ByteSource for MmapSource {
    fn len(&self) -> usize {
        self.mmap.len()
    }

    fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize> {
        let data = self.mmap.as_bytes();
        if out.is_empty() || offset >= data.len() {
            return Ok(0);
        }
        let n = out.len().min(data.len() - offset);
        out[..n].copy_from_slice(&data[offset..offset + n]);
        Ok(n)
    }

    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.mmap.as_bytes())
    }
}

/// Windowed mmap source.
///
/// Maps at most one window at a time. Peak virtual address space is
/// bounded by the window size rather than the file size.
///
/// A window must be at least [`MIN_WINDOW_BYTES`]; smaller values are
/// rejected by the constructor.
#[derive(Debug)]
pub struct WindowedMmapSource {
    file: WindowedMmapFile,
}

impl WindowedMmapSource {
    /// Open `path` with the given bounded window size.
    ///
    /// # Safety
    ///
    /// Same immutable-input contract as
    /// [`WindowedMmapFile::open_path`]: the file must not be modified
    /// while the source is alive.
    pub unsafe fn open_path(path: impl AsRef<Path>, window_bytes: usize) -> io::Result<Self> {
        let file = unsafe { WindowedMmapFile::open_path(path, window_bytes)? };
        Ok(Self { file })
    }
}

impl ByteSource for WindowedMmapSource {
    fn len(&self) -> usize {
        self.file.len()
    }

    fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize> {
        self.file.read_at(offset, out)
    }

    fn as_slice(&self) -> Option<&[u8]> {
        None
    }
}

/// Positional-read source (`pread` / `ReadFile` with offset).
///
/// Never maps the file. Reads are served by the OS; the source is safe
/// to construct and use. If the file is mutated during planning the
/// result is undefined in the sense of a torn read, but there is no
/// memory-safety hazard.
#[derive(Debug)]
pub struct PreadSource {
    file: File,
    len: usize,
}

impl PreadSource {
    /// Open `path` for positional reads.
    ///
    /// On Windows the file is opened with `FILE_SHARE_READ` only, so
    /// other processes cannot open it for writing while planning.
    pub fn open_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(WINDOWS_FILE_SHARE_READ);
        }
        let file = options.open(path)?;
        let len = usize::try_from(file.metadata()?.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "file size does not fit in usize",
            )
        })?;
        Ok(Self { file, len })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_exact_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(out, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(out, offset)
}

impl ByteSource for PreadSource {
    fn len(&self) -> usize {
        self.len
    }

    fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || offset >= self.len {
            return Ok(0);
        }
        let n = out.len().min(self.len - offset);
        let mut total = 0usize;
        while total < n {
            let read = read_exact_at(
                &self.file,
                &mut out[total..n],
                u64::try_from(offset + total).unwrap_or(u64::MAX),
            )?;
            if read == 0 {
                break;
            }
            total += read;
        }
        Ok(total)
    }

    fn as_slice(&self) -> Option<&[u8]> {
        None
    }
}

/// Backend selection for [`plan_partition_ranges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceMode {
    /// Full-file mmap; delegates to the slice scanner.
    #[default]
    Mmap,
    /// Bounded moving-window mmap.
    Windowed,
    /// Positional reads, no mapping.
    Pread,
}

impl SourceMode {
    /// Stable lowercase name used in manifests and CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mmap => "mmap",
            Self::Windowed => "windowed",
            Self::Pread => "pread",
        }
    }
}

/// Options for [`plan_partition_ranges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerOptions {
    /// Backend to use.
    pub mode: SourceMode,
    /// Window size for [`SourceMode::Windowed`]; ignored otherwise.
    pub window_bytes: usize,
    /// Buffer size for buffered backends. Larger values reduce syscall
    /// or copy overhead; the value is raised automatically when needed
    /// to hold the delimiter pattern.
    pub scan_buffer_bytes: usize,
}

impl Default for PlannerOptions {
    fn default() -> Self {
        Self {
            mode: SourceMode::Mmap,
            window_bytes: DEFAULT_WINDOW_BYTES,
            scan_buffer_bytes: DEFAULT_SCAN_BUFFER_BYTES,
        }
    }
}

impl PlannerOptions {
    /// Create options for the given backend with default sizes.
    pub fn new(mode: SourceMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Set the window size used by [`SourceMode::Windowed`].
    pub fn with_window_bytes(mut self, window_bytes: usize) -> Self {
        self.window_bytes = window_bytes;
        self
    }

    /// Set the scan buffer size used by buffered backends.
    pub fn with_scan_buffer_bytes(mut self, scan_buffer_bytes: usize) -> Self {
        self.scan_buffer_bytes = scan_buffer_bytes;
        self
    }
}

/// Find the first occurrence of `delimiter` at an absolute offset
/// `>= from`, reading through `source` in `buffer`-sized steps.
///
/// Buffers overlap by `delimiter.len() - 1` bytes so a pattern that
/// spans two reads is still found. Returns `None` when the pattern does
/// not occur in `[from, source.len())`.
pub(crate) fn find_pattern_from(
    source: &dyn ByteSource,
    from: usize,
    delimiter: &[u8],
    buffer: &mut [u8],
) -> io::Result<Option<usize>> {
    let file_len = source.len();
    let dlen = delimiter.len();
    debug_assert!(dlen > 0);
    debug_assert!(buffer.len() >= dlen);

    let mut offset = from;
    loop {
        if offset >= file_len {
            return Ok(None);
        }

        // Fill the window; sources may return short reads (io::Read
        // semantics), so accumulate until the window is full or the
        // source is exhausted. Without this, a dribble source would
        // starve the pattern matcher: a 1-byte read can never hold a
        // multi-byte delimiter.
        let want = buffer.len().min(file_len - offset);
        let mut filled = 0usize;
        while filled < want {
            let read = source.read_at(offset + filled, &mut buffer[filled..want])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        let read = filled;
        if read == 0 {
            return Ok(None);
        }

        if let Some(position) = scanner::find_pattern_in_slice(&buffer[..read], delimiter) {
            return Ok(Some(offset + position));
        }

        if offset + read >= file_len {
            return Ok(None);
        }

        // Keep the last `dlen - 1` bytes as overlap so a pattern
        // starting inside this buffer but ending in the next is not
        // missed. `max(1)` guarantees forward progress even when a
        // source returns fewer bytes than the overlap.
        let overlap = dlen - 1;
        let advance = read.saturating_sub(overlap).max(1);
        offset = offset.saturating_add(advance);
    }
}

/// Compute N record-aligned partition boundaries from any byte source.
///
/// Identical semantics to
/// [`scanner::find_partition_boundaries_pattern`]: each non-final
/// partition ends after a complete delimiter, an ideal target that
/// already follows a complete delimiter is accepted exactly, and the
/// ranges cover `[0, file_len)` without gaps or overlaps. Sources that
/// expose a complete slice delegate directly to the slice scanner; all
/// other sources use a buffered forward search with exact parity.
///
/// The `scan_buffer_bytes` value is clamped up to the delimiter length
/// so every search window can hold at least one complete pattern.
///
/// # Panics
///
/// Panics if `delimiter` is empty.
pub fn plan_partition_boundaries(
    source: &dyn ByteSource,
    num_partitions: usize,
    delimiter: &[u8],
    scan_buffer_bytes: usize,
) -> io::Result<Vec<(usize, usize)>> {
    assert!(!delimiter.is_empty(), "delimiter must not be empty");

    if let Some(data) = source.as_slice() {
        return Ok(scanner::find_partition_boundaries_pattern(
            data,
            num_partitions,
            delimiter,
        ));
    }

    let file_len = source.len();
    if file_len == 0 || num_partitions == 0 {
        return Ok(Vec::new());
    }
    if num_partitions == 1 {
        return Ok(vec![(0, file_len)]);
    }

    let dlen = delimiter.len();
    let n = num_partitions.min(file_len);
    let mut buffer = vec![0u8; scan_buffer_bytes.max(dlen).max(1)];

    let mut boundaries = Vec::new();
    let mut last_boundary: usize = 0;

    for i in 1..n {
        // Overflow-safe: use u128 intermediate for multiplication.
        let target = ((file_len as u128) * (i as u128) / (n as u128)) as usize;
        if target <= last_boundary {
            continue;
        }

        // Exact-target acceptance (mirrors the slice planner): when the
        // `dlen` bytes immediately before `target` are a complete
        // delimiter, the target is already a boundary. The tail read
        // tolerates short reads so dribble sources stay exact too.
        if target >= dlen {
            let mut tail_read = 0usize;
            while tail_read < dlen {
                let read =
                    source.read_at(target - dlen + tail_read, &mut buffer[tail_read..dlen])?;
                if read == 0 {
                    break;
                }
                tail_read += read;
            }
            if tail_read == dlen && buffer[..dlen] == delimiter[..] {
                boundaries.push(target);
                last_boundary = target;
                continue;
            }
        }

        match find_pattern_from(source, target, delimiter, &mut buffer)? {
            Some(position) => {
                let boundary = position.saturating_add(dlen).min(file_len);
                boundaries.push(boundary);
                last_boundary = boundary;
            }
            None => {
                boundaries.push(file_len);
                break;
            }
        }
    }

    Ok(scanner::ranges_from_boundaries(file_len, &boundaries))
}

/// Plan record-aligned partition ranges for the file at `path` using
/// the selected [`ByteSource`] backend.
///
/// This is the source-selectable entry point behind the CLI and the
/// C ABI planning function. All backends produce identical ranges for
/// the same inputs; the mode only changes how bytes are accessed.
///
/// # Safety
///
/// The caller must ensure that the file is not modified, truncated, or
/// deleted while this function runs (mmap-backed modes; see
/// [`MmapFile::open_path`]). The pread mode has no memory-safety
/// hazard but may observe torn data if the file is mutated.
pub unsafe fn plan_partition_ranges(
    path: impl AsRef<Path>,
    num_partitions: usize,
    delimiter: &[u8],
    options: &PlannerOptions,
) -> io::Result<Vec<(usize, usize)>> {
    assert!(!delimiter.is_empty(), "delimiter must not be empty");

    if num_partitions == 0 {
        return Ok(Vec::new());
    }

    let buffer = options.scan_buffer_bytes.max(1);
    match options.mode {
        SourceMode::Mmap => {
            let source = unsafe { MmapSource::open_path(path)? };
            plan_partition_boundaries(&source, num_partitions, delimiter, buffer)
        }
        SourceMode::Windowed => {
            let source = unsafe { WindowedMmapSource::open_path(path, options.window_bytes)? };
            plan_partition_boundaries(&source, num_partitions, delimiter, buffer)
        }
        SourceMode::Pread => {
            let source = PreadSource::open_path(path)?;
            plan_partition_boundaries(&source, num_partitions, delimiter, buffer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(label: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mmap_chunker_source_{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn cleanup(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    struct DribbleSource {
        data: Vec<u8>,
        max_read: usize,
    }

    impl ByteSource for DribbleSource {
        fn len(&self) -> usize {
            self.data.len()
        }

        fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize> {
            if offset >= self.data.len() || out.is_empty() {
                return Ok(0);
            }
            let n = out.len().min(self.max_read).min(self.data.len() - offset);
            out[..n].copy_from_slice(&self.data[offset..offset + n]);
            Ok(n)
        }

        fn as_slice(&self) -> Option<&[u8]> {
            None
        }
    }

    fn assert_sources_match(
        content: &[u8],
        parts: usize,
        delimiter: &[u8],
        label: &str,
    ) -> Vec<(usize, usize)> {
        let expected = scanner::find_partition_boundaries_pattern(content, parts, delimiter);

        let mmap_source = DribbleSource {
            data: content.to_vec(),
            max_read: usize::MAX,
        };
        let actual = plan_partition_boundaries(&mmap_source, parts, delimiter, 16).unwrap();
        assert_eq!(actual, expected, "{label}: buffered mismatch");
        assert!(actual.len() <= parts.max(1));

        let mut covered = 0usize;
        for &(start, end) in &actual {
            assert_eq!(start, covered, "{label}: gap or overlap");
            assert!(end > start, "{label}: empty range");
            covered = end;
        }
        assert_eq!(covered, content.len(), "{label}: incomplete coverage");
        for (index, &(_, end)) in actual.iter().enumerate() {
            if index + 1 < actual.len() {
                assert_eq!(
                    &content[end - delimiter.len()..end],
                    delimiter,
                    "{label}: range {index} did not end on the delimiter"
                );
            }
        }

        expected
    }

    #[test]
    fn dribble_source_matches_reference_for_patterns() {
        let content = b"a\r\nbb\r\nccc\r\ndddd\r\neeeee\r\nffff\r\n";
        for parts in [1usize, 2, 3, 8, 64] {
            for delimiter in [&b"\n"[..], &b"\r\n"[..], &b"\r\n\r\n"[..]] {
                assert_sources_match(content, parts, delimiter, "dribble");
            }
        }
    }

    #[test]
    fn dribble_source_matches_reference_for_generated_content() {
        let mut content = Vec::new();
        for i in 0..500u32 {
            let len = (i % 23) as usize;
            content.extend(std::iter::repeat(b'a' + (i % 26) as u8).take(len));
            if i % 3 == 0 {
                content.extend_from_slice(b"\r\n");
            } else {
                content.push(b'\n');
            }
        }
        for parts in [1usize, 2, 5, 17, 100, 1000] {
            assert_sources_match(&content, parts, b"\r\n", "generated");
            assert_sources_match(&content, parts, b"\n", "generated-single");
        }
    }

    #[test]
    fn dribble_source_matches_reference_with_tiny_reads() {
        // Single-byte reads defeat the overlap fast path and the tail
        // fast path; parity must still hold via the progress guarantees.
        let content = b"a\r\nbb\r\nccc\r\ndddd\r\n";
        for parts in [1usize, 2, 3, 4, 8] {
            for delimiter in [&b"\n"[..], &b"\r\n"[..]] {
                let source = DribbleSource {
                    data: content.to_vec(),
                    max_read: 1,
                };
                let expected =
                    scanner::find_partition_boundaries_pattern(content, parts, delimiter);
                let actual = plan_partition_boundaries(&source, parts, delimiter, 8).unwrap();
                assert_eq!(actual, expected, "tiny-read mismatch for {delimiter:?}");
            }
        }
    }

    #[test]
    fn dribble_source_edge_cases() {
        assert_eq!(
            plan_partition_boundaries(
                &DribbleSource {
                    data: Vec::new(),
                    max_read: 1
                },
                4,
                b"\r\n",
                8
            )
            .unwrap(),
            Vec::new()
        );
        assert_eq!(
            plan_partition_boundaries(
                &DribbleSource {
                    data: b"x".to_vec(),
                    max_read: 1
                },
                0,
                b"\r\n",
                8
            )
            .unwrap(),
            Vec::new()
        );
        assert_eq!(
            plan_partition_boundaries(
                &DribbleSource {
                    data: b"only record".to_vec(),
                    max_read: 2
                },
                8,
                b"\r\n",
                8
            )
            .unwrap(),
            vec![(0, 11)]
        );
    }

    #[test]
    fn plan_partition_ranges_all_modes_match_slice_scanner() {
        let content = b"alpha\r\nbeta\r\ngamma\r\ndelta\r\nepsilon\r\n";
        let path = temp_file("modes", content);
        let expected = scanner::find_partition_boundaries_pattern(content, 3, b"\r\n");

        for mode in [SourceMode::Mmap, SourceMode::Windowed, SourceMode::Pread] {
            let options = PlannerOptions::new(mode)
                .with_window_bytes(MIN_WINDOW_BYTES)
                .with_scan_buffer_bytes(7);
            let ranges = unsafe { plan_partition_ranges(&path, 3, b"\r\n", &options) }.unwrap();
            assert_eq!(ranges, expected, "mode {mode:?}");
        }

        cleanup(&path);
    }

    #[test]
    fn windowed_and_pread_handle_files_larger_than_window() {
        let mut content = Vec::new();
        for i in 0..20_000u32 {
            content.extend_from_slice(format!("record-{i:06}\r\n").as_bytes());
        }
        let path = temp_file("large", &content);
        let expected = scanner::find_partition_boundaries_pattern(&content, 7, b"\r\n");

        for mode in [SourceMode::Windowed, SourceMode::Pread] {
            let options = PlannerOptions::new(mode)
                .with_window_bytes(MIN_WINDOW_BYTES)
                .with_scan_buffer_bytes(MIN_WINDOW_BYTES);
            let ranges = unsafe { plan_partition_ranges(&path, 7, b"\r\n", &options) }.unwrap();
            assert_eq!(ranges, expected, "mode {mode:?}");
        }

        cleanup(&path);
    }

    #[test]
    fn plan_partition_ranges_matches_single_byte_scanner() {
        let content = b"a\nb\nc\nd\ne\nf\ng\n";
        let path = temp_file("single", content);
        let expected = scanner::find_partition_boundaries(content, 3, b'\n');

        for mode in [SourceMode::Mmap, SourceMode::Windowed, SourceMode::Pread] {
            let options = PlannerOptions::new(mode).with_window_bytes(MIN_WINDOW_BYTES);
            let ranges = unsafe { plan_partition_ranges(&path, 3, b"\n", &options) }.unwrap();
            assert_eq!(ranges, expected, "mode {mode:?}");
        }

        cleanup(&path);
    }

    #[test]
    fn plan_partition_ranges_zero_partitions_is_empty() {
        let path = temp_file("zero", b"a\r\nb\r\n");
        for mode in [SourceMode::Mmap, SourceMode::Windowed, SourceMode::Pread] {
            let options = PlannerOptions::new(mode);
            let ranges = unsafe { plan_partition_ranges(&path, 0, b"\r\n", &options) }.unwrap();
            assert!(ranges.is_empty(), "mode {mode:?}");
        }
        cleanup(&path);
    }

    #[test]
    fn plan_partition_ranges_empty_file_is_empty() {
        let path = temp_file("empty", b"");
        for mode in [SourceMode::Mmap, SourceMode::Windowed, SourceMode::Pread] {
            let options = PlannerOptions::new(mode).with_window_bytes(MIN_WINDOW_BYTES);
            let ranges = unsafe { plan_partition_ranges(&path, 8, b"\r\n", &options) }.unwrap();
            assert!(ranges.is_empty(), "mode {mode:?}");
        }
        cleanup(&path);
    }

    #[test]
    #[should_panic(expected = "delimiter must not be empty")]
    fn plan_partition_ranges_rejects_empty_delimiter() {
        let path = temp_file("empty_delim", b"data");
        let options = PlannerOptions::default();
        let _ = unsafe { plan_partition_ranges(&path, 2, b"", &options) };
    }
}
