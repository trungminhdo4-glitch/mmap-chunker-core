//! Pluggable record-framing strategies.
//!
//! `mmap-chunker` plans ranges over *records*, but what constitutes a
//! record is format-specific. A [`FramingStrategy`] answers one
//! question — *where does the next record end?* — and the planner stays
//! format-agnostic. Built-in strategies:
//!
//! | Strategy | Record boundary |
//! |----------|-----------------|
//! | [`BuiltinFraming::Delimiter`] | immediately after a delimiter pattern |
//! | [`BuiltinFraming::FixedWidth`] | at every `record_bytes` multiple |
//! | [`BuiltinFraming::LengthPrefixed`] | after a length prefix + payload |
//!
//! External code can implement [`FramingStrategy`] for custom/stateful
//! formats (varint framing, container headers, ...) and pass it to
//! [`plan_partition_boundaries_with`](crate::source::plan_partition_boundaries_with).
//!
//! # Boundary contract
//!
//! A [`BoundaryScanner`] returns the *smallest record boundary strictly
//! greater than* a monotonically non-decreasing floor. The planner calls
//! it with increasing absolute targets and treats `None` as "no further
//! record boundary before EOF", in which case the remainder of the file
//! is the final record.
//!
//! `LengthPrefixed` scanners are stateful: they walk records from a
//! cached cursor, so planning is O(file size + partitions), not
//! O(partitions × file size).

use std::fmt;
use std::io;

use crate::source::{find_pattern_from, ByteSource};

/// Maximum supported length-prefix width in bytes (u64).
pub const MAX_LENGTH_PREFIX_BYTES: u8 = 8;

/// A strategy that knows where records end.
///
/// Implementations must be `Send + Sync` because planning may create
/// scanners on worker threads. The descriptor returned by
/// [`describe`](FramingStrategy::describe) is recorded in plan and index
/// manifests.
pub trait FramingStrategy: Send + Sync {
    /// Machine-readable description recorded in manifests.
    fn describe(&self) -> FramingDescriptor;

    /// Minimum buffer size a scanner needs. The planner raises its scan
    /// buffer to at least this value.
    fn minimum_buffer_bytes(&self) -> usize {
        1
    }

    /// Create a stateful boundary scanner over `source` using `buffer`
    /// as scratch space.
    fn scanner<'a>(
        &'a self,
        source: &'a dyn ByteSource,
        buffer: &'a mut [u8],
    ) -> Box<dyn BoundaryScanner + 'a>;
}

/// Stateful boundary search over a [`ByteSource`].
pub trait BoundaryScanner {
    /// Return the smallest record boundary strictly greater than `from`.
    ///
    /// `from` values increase across calls. Returning `None` means no
    /// further complete record boundary exists; the caller treats the
    /// file remainder as the last record.
    fn boundary_after(&mut self, from: usize) -> io::Result<Option<usize>>;
}

/// Serializable framing description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramingDescriptor {
    /// Raw delimiter pattern (single or multi byte).
    Delimiter { delimiter: Vec<u8> },
    /// Fixed-size records.
    FixedWidth { record_bytes: usize },
    /// Length-prefixed binary records.
    LengthPrefixed {
        prefix_bytes: u8,
        little_endian: bool,
        length_includes_prefix: bool,
    },
    /// An external strategy that the planner cannot reconstruct by name.
    Custom { name: String },
}

impl FramingDescriptor {
    /// Stable lowercase strategy name used in manifests.
    pub fn strategy_name(&self) -> &'static str {
        match self {
            Self::Delimiter { .. } => "delimiter",
            Self::FixedWidth { .. } => "fixed_width",
            Self::LengthPrefixed { .. } => "length_prefixed",
            Self::Custom { .. } => "custom",
        }
    }

    /// Reconstruct the built-in strategy for this descriptor.
    ///
    /// Returns `None` for [`FramingDescriptor::Custom`].
    pub fn try_as_builtin(&self) -> io::Result<Option<BuiltinFraming>> {
        match self {
            Self::Delimiter { delimiter } => {
                Ok(Some(BuiltinFraming::delimiter(delimiter.clone())?))
            }
            Self::FixedWidth { record_bytes } => {
                Ok(Some(BuiltinFraming::fixed_width(*record_bytes)?))
            }
            Self::LengthPrefixed {
                prefix_bytes,
                little_endian,
                length_includes_prefix,
            } => Ok(Some(BuiltinFraming::length_prefixed(
                *prefix_bytes,
                *little_endian,
                *length_includes_prefix,
            )?)),
            Self::Custom { .. } => Ok(None),
        }
    }
}

impl fmt::Display for FramingDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delimiter { delimiter } => {
                write!(formatter, "delimiter(")?;
                for byte in delimiter {
                    write!(formatter, "{byte:02x}")?;
                }
                write!(formatter, ")")
            }
            Self::FixedWidth { record_bytes } => {
                write!(formatter, "fixed_width({record_bytes})")
            }
            Self::LengthPrefixed {
                prefix_bytes,
                little_endian,
                length_includes_prefix,
            } => write!(
                formatter,
                "length_prefixed(prefix={prefix_bytes}, endian={}, includes_prefix={length_includes_prefix})",
                if *little_endian { "le" } else { "be" }
            ),
            Self::Custom { name } => write!(formatter, "custom({name})"),
        }
    }
}

/// Built-in framing strategies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinFraming {
    /// Records end immediately after a delimiter pattern.
    Delimiter(Vec<u8>),
    /// Records are exactly `record_bytes` long (the final record may be
    /// shorter and ends at EOF).
    FixedWidth { record_bytes: usize },
    /// Records are a length prefix followed by that many payload bytes.
    LengthPrefixed {
        prefix_bytes: u8,
        little_endian: bool,
        length_includes_prefix: bool,
    },
}

impl BuiltinFraming {
    /// Delimiter framing; rejects an empty delimiter.
    pub fn delimiter(delimiter: impl Into<Vec<u8>>) -> io::Result<Self> {
        let delimiter = delimiter.into();
        if delimiter.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "delimiter must not be empty",
            ));
        }
        Ok(Self::Delimiter(delimiter))
    }

    /// Fixed-width framing; rejects `record_bytes == 0`.
    pub fn fixed_width(record_bytes: usize) -> io::Result<Self> {
        if record_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record_bytes must be > 0",
            ));
        }
        Ok(Self::FixedWidth { record_bytes })
    }

    /// Length-prefixed framing; `prefix_bytes` must be in `1..=8`.
    pub fn length_prefixed(
        prefix_bytes: u8,
        little_endian: bool,
        length_includes_prefix: bool,
    ) -> io::Result<Self> {
        if prefix_bytes == 0 || prefix_bytes > MAX_LENGTH_PREFIX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prefix_bytes must be in 1..=8",
            ));
        }
        Ok(Self::LengthPrefixed {
            prefix_bytes,
            little_endian,
            length_includes_prefix,
        })
    }
}

impl FramingStrategy for BuiltinFraming {
    fn describe(&self) -> FramingDescriptor {
        match self {
            Self::Delimiter(delimiter) => FramingDescriptor::Delimiter {
                delimiter: delimiter.clone(),
            },
            Self::FixedWidth { record_bytes } => FramingDescriptor::FixedWidth {
                record_bytes: *record_bytes,
            },
            Self::LengthPrefixed {
                prefix_bytes,
                little_endian,
                length_includes_prefix,
            } => FramingDescriptor::LengthPrefixed {
                prefix_bytes: *prefix_bytes,
                little_endian: *little_endian,
                length_includes_prefix: *length_includes_prefix,
            },
        }
    }

    fn minimum_buffer_bytes(&self) -> usize {
        match self {
            Self::Delimiter(delimiter) => delimiter.len(),
            Self::FixedWidth { .. } => 1,
            Self::LengthPrefixed { prefix_bytes, .. } => usize::from(*prefix_bytes),
        }
    }

    fn scanner<'a>(
        &'a self,
        source: &'a dyn ByteSource,
        buffer: &'a mut [u8],
    ) -> Box<dyn BoundaryScanner + 'a> {
        match self {
            Self::Delimiter(delimiter) => Box::new(DelimiterScanner {
                source,
                buffer,
                delimiter,
            }),
            Self::FixedWidth { record_bytes } => Box::new(FixedWidthScanner {
                file_len: source.len(),
                record_bytes: *record_bytes,
            }),
            Self::LengthPrefixed {
                prefix_bytes,
                little_endian,
                length_includes_prefix,
            } => Box::new(LengthPrefixedScanner {
                source,
                buffer,
                cursor: 0,
                window_start: 0,
                window_len: 0,
                prefix_bytes: *prefix_bytes,
                little_endian: *little_endian,
                length_includes_prefix: *length_includes_prefix,
            }),
        }
    }
}

/// Delimiter scanner: first complete pattern at or after the floor.
struct DelimiterScanner<'a> {
    source: &'a dyn ByteSource,
    buffer: &'a mut [u8],
    delimiter: &'a [u8],
}

impl BoundaryScanner for DelimiterScanner<'_> {
    fn boundary_after(&mut self, from: usize) -> io::Result<Option<usize>> {
        let file_len = self.source.len();
        if from >= file_len {
            return Ok(None);
        }
        match find_pattern_from(self.source, from, self.delimiter, self.buffer)? {
            Some(position) => Ok(Some(
                position.saturating_add(self.delimiter.len()).min(file_len),
            )),
            None => Ok(None),
        }
    }
}

/// Fixed-width scanner: next record boundary is the next multiple of
/// `record_bytes` strictly greater than the floor.
struct FixedWidthScanner {
    file_len: usize,
    record_bytes: usize,
}

impl BoundaryScanner for FixedWidthScanner {
    fn boundary_after(&mut self, from: usize) -> io::Result<Option<usize>> {
        if from >= self.file_len {
            return Ok(None);
        }
        let next = ((from as u128 / self.record_bytes as u128) + 1) * self.record_bytes as u128;
        if next > self.file_len as u128 {
            Ok(None)
        } else {
            Ok(Some(next as usize))
        }
    }
}

/// Length-prefixed scanner with a cached parse window.
struct LengthPrefixedScanner<'a> {
    source: &'a dyn ByteSource,
    buffer: &'a mut [u8],
    cursor: usize,
    window_start: usize,
    window_len: usize,
    prefix_bytes: u8,
    little_endian: bool,
    length_includes_prefix: bool,
}

impl LengthPrefixedScanner<'_> {
    /// Ensure the prefix at `cursor` is inside the cached window.
    ///
    /// Returns `Ok(false)` when fewer than `prefix_bytes` bytes remain.
    fn ensure_prefix(&mut self) -> io::Result<bool> {
        let file_len = self.source.len();
        let prefix_bytes = usize::from(self.prefix_bytes);
        let needed_end = self.cursor.saturating_add(prefix_bytes);
        if self.cursor >= self.window_start
            && needed_end <= self.window_start.saturating_add(self.window_len)
        {
            return Ok(true);
        }

        let want = self.buffer.len().min(file_len.saturating_sub(self.cursor));
        if want < prefix_bytes {
            return Ok(false);
        }
        let read = self.source.read_at(self.cursor, &mut self.buffer[..want])?;
        self.window_start = self.cursor;
        self.window_len = read;
        Ok(read >= prefix_bytes)
    }

    fn read_prefix(&self) -> u64 {
        let prefix_bytes = usize::from(self.prefix_bytes);
        let relative = self.cursor - self.window_start;
        let bytes = &self.buffer[relative..relative + prefix_bytes];
        let mut value = 0u64;
        if self.little_endian {
            for index in (0..prefix_bytes).rev() {
                value = (value << 8) | u64::from(bytes[index]);
            }
        } else {
            for &byte in bytes {
                value = (value << 8) | u64::from(byte);
            }
        }
        value
    }

    fn record_end(&self) -> io::Result<usize> {
        let file_len = self.source.len();
        let prefix_bytes = u64::from(self.prefix_bytes);
        let prefix = self.read_prefix();
        let record_len = if self.length_includes_prefix {
            if prefix < prefix_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "length prefix is smaller than the prefix size",
                ));
            }
            prefix
        } else {
            prefix.checked_add(prefix_bytes).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "record length overflow")
            })?
        };

        let end = u128::try_from(self.cursor)
            .unwrap_or(u128::MAX)
            .saturating_add(u128::from(record_len));
        if end > file_len as u128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "length-prefixed record exceeds file size",
            ));
        }
        usize::try_from(end)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record end exceeds usize"))
    }
}

impl BoundaryScanner for LengthPrefixedScanner<'_> {
    fn boundary_after(&mut self, from: usize) -> io::Result<Option<usize>> {
        let file_len = self.source.len();
        loop {
            if self.cursor >= file_len {
                return Ok(None);
            }
            if !self.ensure_prefix()? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated length prefix at end of file",
                ));
            }
            let end = self.record_end()?;
            if end <= self.cursor {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "length-prefixed record does not advance",
                ));
            }
            self.cursor = end;
            if end > from {
                return Ok(Some(end));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::PreadSource;

    fn records(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    struct SliceLike {
        data: Vec<u8>,
    }

    impl ByteSource for SliceLike {
        fn len(&self) -> usize {
            self.data.len()
        }

        fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize> {
            if offset >= self.data.len() || out.is_empty() {
                return Ok(0);
            }
            let n = out.len().min(self.data.len() - offset);
            out[..n].copy_from_slice(&self.data[offset..offset + n]);
            Ok(n)
        }

        fn as_slice(&self) -> Option<&[u8]> {
            None
        }
    }

    fn collect_boundaries(
        strategy: &dyn FramingStrategy,
        source: &dyn ByteSource,
        buffer_bytes: usize,
    ) -> io::Result<Vec<usize>> {
        let mut buffer = vec![0u8; buffer_bytes.max(strategy.minimum_buffer_bytes())];
        let mut scanner = strategy.scanner(source, &mut buffer);
        let mut boundaries = Vec::new();
        let mut floor = 0usize;
        while let Some(boundary) = scanner.boundary_after(floor)? {
            assert!(boundary > floor);
            boundaries.push(boundary);
            floor = boundary;
        }
        Ok(boundaries)
    }

    #[test]
    fn delimiter_framing_yields_pattern_ends() {
        let data = records(&[b"a\r\n", b"bb\r\n", b"ccc\r\n"]);
        let source = SliceLike { data };
        let strategy = BuiltinFraming::delimiter(b"\r\n".to_vec()).unwrap();
        let boundaries = collect_boundaries(&strategy, &source, 4).unwrap();
        assert_eq!(boundaries, vec![3, 7, 12]);
    }

    #[test]
    fn delimiter_framing_matches_slice_scanner() {
        let data = records(&[b"a,", b"cc,d,", b"eee"]);
        let expected = crate::scanner::find_partition_boundaries_pattern(&data, 2, b",");
        let source = SliceLike { data };
        let strategy = BuiltinFraming::delimiter(b",").unwrap();
        let boundaries = collect_boundaries(&strategy, &source, 3).unwrap();
        assert_eq!(boundaries, vec![2, 5, 7]);
        let ranges =
            crate::source::plan_partition_boundaries_with(&source, 2, &strategy, 3).unwrap();
        assert_eq!(ranges, expected);
    }

    #[test]
    fn fixed_width_boundaries_are_multiples() {
        let data = vec![b'x'; 100];
        let source = SliceLike { data };
        let strategy = BuiltinFraming::fixed_width(32).unwrap();
        let boundaries = collect_boundaries(&strategy, &source, 8).unwrap();
        assert_eq!(boundaries, vec![32, 64, 96]);
    }

    #[test]
    fn fixed_width_partitions_are_record_aligned() {
        let data: Vec<u8> = (0..100u8).collect();
        let source = SliceLike { data };
        let strategy = BuiltinFraming::fixed_width(16).unwrap();
        let ranges =
            crate::source::plan_partition_boundaries_with(&source, 4, &strategy, 16).unwrap();
        let mut cursor = 0;
        for (index, (start, end)) in ranges.iter().enumerate() {
            assert_eq!(*start, cursor);
            if index + 1 < ranges.len() {
                assert_eq!(end % 16, 0);
            }
            cursor = *end;
        }
        assert_eq!(cursor, 100);
    }

    #[test]
    fn fixed_width_rejects_zero_record_bytes() {
        assert!(BuiltinFraming::fixed_width(0).is_err());
    }

    fn length_records(payloads: &[&[u8]], prefix_bytes: u8, little_endian: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for payload in payloads {
            let length = payload.len() as u64;
            let prefix = if little_endian {
                length.to_le_bytes()
            } else {
                length.to_be_bytes()
            };
            out.extend_from_slice(&prefix[..usize::from(prefix_bytes)]);
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn length_prefixed_scans_across_tiny_buffers() {
        let payloads: &[&[u8]] = &[b"alpha", b"be", b"gamma-payload", b""];
        let data = length_records(payloads, 2, true);
        let source = SliceLike { data: data.clone() };
        let strategy = BuiltinFraming::length_prefixed(2, true, false).unwrap();

        let expected: Vec<usize> = payloads
            .iter()
            .scan(0usize, |cursor, payload| {
                *cursor += 2 + payload.len();
                Some(*cursor)
            })
            .collect();
        assert_eq!(collect_boundaries(&strategy, &source, 3).unwrap(), expected);

        let ranges =
            crate::source::plan_partition_boundaries_with(&source, 3, &strategy, 3).unwrap();
        assert_eq!(ranges.last().unwrap().1, data.len());
        assert!(ranges.len() <= 3);
    }

    #[test]
    fn length_prefixed_big_endian_and_includes_prefix() {
        let payloads: &[&[u8]] = &[b"one", b"two", b"three"];
        let mut data = Vec::new();
        for payload in payloads {
            let total = 4 + payload.len() as u32;
            data.extend_from_slice(&total.to_be_bytes());
            data.extend_from_slice(payload);
        }
        let source = SliceLike { data };
        let strategy = BuiltinFraming::length_prefixed(4, false, true).unwrap();
        let boundaries = collect_boundaries(&strategy, &source, 64).unwrap();
        assert_eq!(boundaries, vec![7, 14, 23]);
    }

    #[test]
    fn length_prefixed_uses_pread_source() {
        let dir = std::env::temp_dir().join("mmap_chunker_framing_length_pread");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("records.bin");
        let data = length_records(&[b"abc", b"de", b"fghi"], 1, true);
        std::fs::write(&path, &data).unwrap();

        let source = PreadSource::open_path(&path).unwrap();
        let strategy = BuiltinFraming::length_prefixed(1, true, false).unwrap();
        let boundaries = collect_boundaries(&strategy, &source, 2).unwrap();
        assert_eq!(boundaries, vec![4, 7, 12]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn length_prefixed_rejects_truncated_record() {
        let data = vec![5u8, b'a', b'b'];
        let source = SliceLike { data };
        let strategy = BuiltinFraming::length_prefixed(1, true, false).unwrap();
        let error = collect_boundaries(&strategy, &source, 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn length_prefixed_rejects_invalid_prefix_width() {
        assert!(BuiltinFraming::length_prefixed(0, true, false).is_err());
        assert!(BuiltinFraming::length_prefixed(9, true, false).is_err());
        assert!(BuiltinFraming::length_prefixed(8, true, false).is_ok());
    }

    #[test]
    fn descriptor_round_trips_through_builtin() {
        let strategies = vec![
            BuiltinFraming::delimiter(b"\r\n".to_vec()).unwrap(),
            BuiltinFraming::fixed_width(512).unwrap(),
            BuiltinFraming::length_prefixed(4, false, true).unwrap(),
        ];
        for strategy in strategies {
            let descriptor = strategy.describe();
            let rebuilt = descriptor.try_as_builtin().unwrap().unwrap();
            assert_eq!(rebuilt.describe(), descriptor);
        }

        let custom = FramingDescriptor::Custom {
            name: "varint".to_owned(),
        };
        assert_eq!(custom.strategy_name(), "custom");
        assert!(custom.try_as_builtin().unwrap().is_none());
    }
}
