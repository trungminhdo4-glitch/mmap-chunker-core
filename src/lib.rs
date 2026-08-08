pub mod ffi;
pub mod mmap;
pub mod scanner;

pub use ffi::{
    CChunkView, CEngineHandle, ABI_VERSION, CAP_CONFIGURABLE_DELIMITER, CAP_ERROR_STRINGS,
    CAP_FIXED_SIZE_CHUNKING, CAP_RECORD_PARTITIONING, CAP_ZERO_COPY,
};
pub use mmap::MmapFile;
pub use scanner::ChunkCursor;
pub use scanner::PatternChunkCursor;

#[derive(Debug)]
pub(crate) enum ChunkLayout {
    Empty,
    Delimited(Vec<(usize, usize)>),
    Fixed {
        chunk_size: usize,
        chunk_count: usize,
    },
    Partitioned(Vec<(usize, usize)>),
}

use std::io;
use std::path::Path;

/// Safe Rust interface for memory-mapped file chunking.
///
/// Wraps [`MmapFile`] with a chunk-layout state machine supporting
/// three scan modes: delimited, fixed-size, and record-aligned
/// partition planning.
///
/// # Safety
///
/// The constructor [`MmapChunker::open`] is `unsafe` because
/// file-backed memory mappings can violate Rust's `&[u8]` immutability
/// guarantee if the underlying file is mutated concurrently. Every
/// other method on this type is safe after construction.
///
/// # Example
///
/// ```no_run
/// use std::io;
/// # fn main() -> io::Result<()> {
/// let mut file = unsafe {
///     mmap_chunker_core::MmapChunker::open("records.jsonl")?
/// };
/// let count = file.scan_delimited(64 * 1024, b'\n');
/// for i in 0..count {
///     if let Some(chunk) = file.get_chunk(i) {
///         let _data: &[u8] = chunk;
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct MmapChunker {
    mmap: MmapFile,
    layout: ChunkLayout,
}

impl MmapChunker {
    /// Open and memory-map the file at `path` for read-only chunked
    /// access.
    ///
    /// Accepts any type that converts to `Path` (`&str`, `&Path`,
    /// `PathBuf`, `&OsStr`). On Windows the path is encoded as UTF-16
    /// directly; on Unix the raw OS path bytes are used.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the backing file is not modified,
    /// truncated, deleted, or otherwise invalidated for the entire
    /// lifetime of this `MmapChunker` and all `&[u8]` slices derived
    /// from it (via [`get_chunk`](Self::get_chunk) or
    /// [`as_bytes`](Self::as_bytes)).
    ///
    /// Concurrent file mutation by any process — including the calling
    /// process — violates the immutability guarantee of `&[u8]` and is
    /// Rust undefined behavior.
    ///
    /// On POSIX systems, another process may freely open the same file
    /// for writing. Use external synchronization (file locks,
    /// snapshots, or immutable files) to satisfy this contract. On
    /// Windows, `FILE_SHARE_READ` prevents other processes from
    /// opening the file for writing, but same-process mutation remains
    /// possible.
    pub unsafe fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mmap = MmapFile::open_path(path)?;
        Ok(Self {
            mmap,
            layout: ChunkLayout::Empty,
        })
    }

    /// Returns the number of chunks in the current layout.
    ///
    /// Returns 0 if no scan has been performed or if the file is empty.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        match &self.layout {
            ChunkLayout::Empty => 0,
            ChunkLayout::Delimited(chunks) | ChunkLayout::Partitioned(chunks) => chunks.len(),
            ChunkLayout::Fixed { chunk_count, .. } => *chunk_count,
        }
    }

    /// Scan the file with the given approximate chunk size and
    /// single-byte delimiter.
    ///
    /// Chunk boundaries are placed at or after each `chunk_size`
    /// interval, snapped to the next occurrence of `delimiter`.
    /// The last chunk extends to EOF.
    ///
    /// Replaces any previous layout. Returns the number of chunks.
    pub fn scan_delimited(&mut self, chunk_size: usize, delimiter: u8) -> usize {
        let data = self.mmap.as_bytes();
        if data.is_empty() {
            self.layout = ChunkLayout::Empty;
            return 0;
        }
        let chunks = scanner::find_chunk_boundaries(data, chunk_size, delimiter);
        let count = chunks.len();
        self.layout = ChunkLayout::Delimited(chunks);
        count
    }

    /// Partition the file into sequential fixed-size chunks.
    ///
    /// Chunks are at exact `chunk_size` intervals with the last chunk
    /// potentially shorter at EOF. No delimiter scanning.
    ///
    /// Replaces any previous layout. Returns the number of chunks.
    pub fn scan_fixed(&mut self, chunk_size: usize) -> usize {
        let file_len = self.mmap.len();
        if file_len == 0 {
            self.layout = ChunkLayout::Empty;
            return 0;
        }
        let effective_size = chunk_size.max(1);
        let count = file_len.div_ceil(effective_size);
        self.layout = ChunkLayout::Fixed {
            chunk_size: effective_size,
            chunk_count: count,
        };
        count
    }

    /// Plan record-aligned partition byte ranges for N-way parallel
    /// consumers.
    ///
    /// Computes approximately balanced byte ranges where every
    /// partition boundary falls on a record boundary (immediately
    /// after `delimiter`), ensuring no record is split.
    ///
    /// Actual partition count may be less than `num_partitions` if
    /// giant records span multiple ideal target positions.
    ///
    /// Replaces any previous layout. Returns the number of partitions.
    pub fn partition_records(&mut self, num_partitions: usize, delimiter: u8) -> usize {
        let data = self.mmap.as_bytes();
        let file_len = data.len();
        if file_len == 0 || num_partitions == 0 {
            self.layout = ChunkLayout::Empty;
            return 0;
        }
        let partitions = scanner::find_partition_boundaries(data, num_partitions, delimiter);
        let count = partitions.len();
        self.layout = ChunkLayout::Partitioned(partitions);
        count
    }

    /// Create a lazy streaming cursor for sequential chunk consumption.
    ///
    /// Returns a [`ChunkCursor`] that yields chunks one at a time using
    /// the same delimiter-aware boundary semantics as
    /// [`scan_delimited`](Self::scan_delimited), but without
    /// pre-computing a `Vec` of all boundaries.
    ///
    /// O(1) state (~40 bytes on 64-bit) regardless of file size.
    /// Ideal for low-memory streaming consumers where random access
    /// via [`get_chunk`](Self::get_chunk) is not needed.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mmap_chunker_core::MmapChunker;
    ///
    /// let file = unsafe { MmapChunker::open("records.jsonl")? };
    /// for chunk in file.delimited_cursor(64 * 1024, b'\n') {
    ///     let _data: &[u8] = chunk;
    /// }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[inline]
    pub fn delimited_cursor(&self, chunk_size: usize, delimiter: u8) -> ChunkCursor<'_> {
        ChunkCursor::new(self.as_bytes(), chunk_size, delimiter)
    }

    /// Scan with a multi-byte delimiter (e.g., `b"\r\n"` for CRLF).
    ///
    /// Same semantics as [`scan_delimited`](Self::scan_delimited) but
    /// the delimiter can be multiple bytes. Chunk boundaries are placed
    /// immediately after the complete delimiter.
    ///
    /// When `delimiter.len() == 1`, this produces identical results to
    /// the single-byte path. Delegates to the SWAR fast path internally.
    ///
    /// # Panics
    ///
    /// Panics if `delimiter` is empty.
    pub fn scan_delimited_pattern(&mut self, chunk_size: usize, delimiter: &[u8]) -> usize {
        let data = self.mmap.as_bytes();
        if data.is_empty() {
            self.layout = ChunkLayout::Empty;
            return 0;
        }
        let chunks = scanner::find_chunk_boundaries_pattern(data, chunk_size, delimiter);
        let count = chunks.len();
        self.layout = ChunkLayout::Delimited(chunks);
        count
    }

    /// Create a lazy streaming cursor with a multi-byte delimiter.
    ///
    /// Returns a [`PatternChunkCursor`] — same O(1) memory semantics
    /// as [`delimited_cursor`](Self::delimited_cursor), but for
    /// multi-byte delimiters like `b"\r\n"`.
    ///
    /// # Panics
    ///
    /// Panics if `delimiter` is empty.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mmap_chunker_core::MmapChunker;
    ///
    /// let file = unsafe { MmapChunker::open("records.jsonl")? };
    /// for chunk in file.delimited_cursor_pattern(64 * 1024, b"\r\n") {
    ///     let _data: &[u8] = chunk;
    /// }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[inline]
    pub fn delimited_cursor_pattern<'a>(
        &'a self,
        chunk_size: usize,
        delimiter: &'a [u8],
    ) -> PatternChunkCursor<'a, 'a> {
        PatternChunkCursor::new(self.as_bytes(), chunk_size, delimiter)
    }

    /// Retrieve a zero-copy chunk by index.
    ///
    /// Returns `Some(&[u8])` pointing directly into the mapped file,
    /// or `None` if the index is out of bounds or no scan has been
    /// performed.
    ///
    /// The returned slice is valid for the lifetime of `self`.
    pub fn get_chunk(&self, index: usize) -> Option<&[u8]> {
        let data = self.mmap.as_bytes();
        let (start, end) = match &self.layout {
            ChunkLayout::Empty => return None,
            ChunkLayout::Delimited(chunks) | ChunkLayout::Partitioned(chunks) => {
                *chunks.get(index)?
            }
            ChunkLayout::Fixed {
                chunk_size,
                chunk_count,
            } => {
                if index >= *chunk_count {
                    return None;
                }
                scanner::fixed_chunk_bounds(data.len(), *chunk_size, index)?
            }
        };
        Some(&data[start..end])
    }

    /// Returns the mapped file contents as a byte slice.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.mmap.as_bytes()
    }

    /// Returns the file size in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Returns `true` if the file is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mmap_chunker_core_mc_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("data.txt");
        std::fs::write(&file_path, content).unwrap();
        file_path
    }

    fn cleanup(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn test_chunker_open_nonexistent() {
        unsafe {
            let err = MmapChunker::open("definitely_does_not_exist_12345.dat").unwrap_err();
            assert!(
                err.kind() == std::io::ErrorKind::NotFound
                    || err.kind() == std::io::ErrorKind::Other
            );
        }
    }

    #[test]
    fn test_chunker_open_empty_file() {
        let path = temp_file("empty", b"");

        unsafe {
            let file = MmapChunker::open(&path).unwrap();
            assert!(file.is_empty());
            assert_eq!(file.len(), 0);
            assert_eq!(file.chunk_count(), 0);
            assert_eq!(file.as_bytes(), b"");
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_scan_delimited_basic() {
        let path = temp_file("delimited", b"aaa\nbbb\nccc\nddd\n");

        unsafe {
            let mut file = MmapChunker::open(&path).unwrap();
            let count = file.scan_delimited(4, b'\n');
            assert_eq!(count, 2);
            assert_eq!(file.chunk_count(), 2);

            assert_eq!(file.get_chunk(0), Some(b"aaa\nbbb\n" as &[u8]));
            assert_eq!(file.get_chunk(1), Some(b"ccc\nddd\n" as &[u8]));
            assert_eq!(file.get_chunk(2), None);
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_get_chunk_before_scan() {
        let path = temp_file("prescan", b"some data\n");

        unsafe {
            let file = MmapChunker::open(&path).unwrap();
            assert_eq!(file.chunk_count(), 0);
            assert_eq!(file.get_chunk(0), None);
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_scan_fixed() {
        let path = temp_file("fixed", b"AAAABBBBCCCCDDDD");

        unsafe {
            let mut file = MmapChunker::open(&path).unwrap();
            let count = file.scan_fixed(4);
            assert_eq!(count, 4);
            assert_eq!(file.chunk_count(), 4);

            assert_eq!(file.get_chunk(0), Some(b"AAAA" as &[u8]));
            assert_eq!(file.get_chunk(1), Some(b"BBBB" as &[u8]));
            assert_eq!(file.get_chunk(2), Some(b"CCCC" as &[u8]));
            assert_eq!(file.get_chunk(3), Some(b"DDDD" as &[u8]));
            assert_eq!(file.get_chunk(4), None);
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_scan_fixed_short_last() {
        let path = temp_file("fixed_short", b"XXXXXXXXX");

        unsafe {
            let mut file = MmapChunker::open(&path).unwrap();
            let count = file.scan_fixed(4);
            assert_eq!(count, 3);
            assert_eq!(file.get_chunk(0).map(|c| c.len()), Some(4));
            assert_eq!(file.get_chunk(1).map(|c| c.len()), Some(4));
            assert_eq!(file.get_chunk(2).map(|c| c.len()), Some(1));
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_partition_records() {
        let path = temp_file("partition", b"record1\nrecord2\nrecord3\nrecord4\n");

        unsafe {
            let mut file = MmapChunker::open(&path).unwrap();
            let count = file.partition_records(2, b'\n');
            assert!(count == 2);

            let mut total = 0usize;
            for i in 0..count {
                let chunk = file.get_chunk(i).unwrap();
                total += chunk.len();
                assert!(!chunk.is_empty());
            }
            assert_eq!(total, file.len());
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_as_bytes() {
        let path = temp_file("as_bytes", b"hello world!");

        unsafe {
            let file = MmapChunker::open(&path).unwrap();
            assert_eq!(file.as_bytes(), b"hello world!");
            assert_eq!(file.len(), 12);
            assert!(!file.is_empty());
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_mode_switching() {
        let path = temp_file("mode_switch", b"aaa\nbbb\nccc\nddd\n");

        unsafe {
            let mut file = MmapChunker::open(&path).unwrap();

            let dc = file.scan_delimited(4, b'\n');
            assert!(dc > 0);

            let fc = file.scan_fixed(4);
            assert!(fc > 0);
            assert_eq!(file.chunk_count(), fc);

            let dc2 = file.scan_delimited(4, b'\n');
            assert_eq!(dc2, dc);

            let pc = file.partition_records(2, b'\n');
            assert_eq!(pc, 2);
            assert_eq!(file.chunk_count(), 2);
        }

        cleanup(&path);
    }

    #[test]
    fn test_chunker_large_file() {
        let path = temp_file("large", &vec![b'x'; 100_000]);

        unsafe {
            let mut file = MmapChunker::open(&path).unwrap();
            assert_eq!(file.len(), 100_000);

            let count = file.scan_fixed(4096);
            assert!(count > 0);

            let mut total = 0usize;
            for i in 0..count {
                let chunk = file.get_chunk(i).unwrap();
                total += chunk.len();
            }
            assert_eq!(total, 100_000);
        }

        cleanup(&path);
    }
}
