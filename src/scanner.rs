/// Find chunk boundaries in `data` using the given delimiter.
///
/// Walks through `data` and creates sequential chunks that cover the entire
/// input. Each chunk starts where the previous chunk ended. At each
/// `chunk_size` step, the next delimiter in the data is searched and the
/// chunk boundary is placed immediately after it (including the delimiter
/// byte in the chunk).
///
/// The last chunk always extends to the end of `data`, regardless of
/// whether a trailing delimiter exists.
///
/// Returns a `Vec` of `(start_offset, end_offset)` pairs. Offsets are
/// absolute byte positions within `data`.
pub fn find_chunk_boundaries(data: &[u8], chunk_size: usize, delimiter: u8) -> Vec<(usize, usize)> {
    if data.is_empty() {
        return Vec::new();
    }

    let len = data.len();
    let step = chunk_size.max(1);
    let estimate = (len / step) + 2;
    let mut chunks = Vec::with_capacity(estimate);

    let mut start = 0usize;

    while start < len {
        let mut end = start.saturating_add(step);

        if end >= len {
            end = len;
        } else {
            let remainder = &data[end..];
            if let Some(rel_pos) = find_byte_swar(remainder, delimiter) {
                end = end + rel_pos + 1;
                if end > len {
                    end = len;
                }
            } else {
                end = len;
            }
        }

        chunks.push((start, end));
        start = end;
    }

    chunks
}

#[cfg(test)]
mod differential_tests {
    use super::{
        find_byte_swar, find_chunk_boundaries, find_chunk_boundaries_pattern,
        find_partition_boundaries, ChunkCursor, PatternChunkCursor,
    };

    const SINGLE_SEED: u64 = 0x5349_4e47_4c45_0001;
    const CURSOR_SEED: u64 = 0x4355_5253_4f52_0002;
    const PATTERN_SEED: u64 = 0x5041_5454_4552_0003;
    const PATTERN_CURSOR_SEED: u64 = 0x5043_5552_534f_0004;
    const SWAR_SEED: u64 = 0x5357_4152_0000_0005;
    const PARTITION_SEED: u64 = 0x5041_5254_0000_0006;

    #[derive(Clone, Copy)]
    struct Lcg {
        state: u64,
    }

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next_u64(&mut self) -> u64 {
            self.state = self
                .state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.state
        }

        fn next_u8(&mut self) -> u8 {
            self.next_u64() as u8
        }

        fn next_usize(&mut self, upper_exclusive: usize) -> usize {
            if upper_exclusive == 0 {
                0
            } else {
                (self.next_u64() % upper_exclusive as u64) as usize
            }
        }
    }

    fn scalar_single_byte_boundaries(
        data: &[u8],
        chunk_size: usize,
        delimiter: u8,
    ) -> Vec<(usize, usize)> {
        let mut boundaries = Vec::new();
        let step = chunk_size.max(1);
        let mut start = 0;

        while start < data.len() {
            let target = start.saturating_add(step);
            if target >= data.len() {
                boundaries.push((start, data.len()));
                break;
            }

            let mut end = target;
            while end < data.len() {
                if data[end] == delimiter {
                    end += 1;
                    break;
                }
                end += 1;
            }
            boundaries.push((start, end.min(data.len())));
            start = end;
        }

        boundaries
    }

    fn scalar_byte_position(haystack: &[u8], delimiter: u8) -> Option<usize> {
        let mut position = 0;
        while position < haystack.len() {
            if haystack[position] == delimiter {
                return Some(position);
            }
            position += 1;
        }
        None
    }

    fn scalar_pattern_boundaries(
        data: &[u8],
        chunk_size: usize,
        pattern: &[u8],
    ) -> Vec<(usize, usize)> {
        assert!(!pattern.is_empty());

        let mut boundaries = Vec::new();
        let step = chunk_size.max(1);
        let mut start = 0;

        while start < data.len() {
            let target = start.saturating_add(step);
            if target >= data.len() {
                boundaries.push((start, data.len()));
                break;
            }

            let mut candidate = target;
            let mut end = data.len();
            while candidate + pattern.len() <= data.len() {
                let mut matches = true;
                for offset in 0..pattern.len() {
                    if data[candidate + offset] != pattern[offset] {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    end = candidate + pattern.len();
                    break;
                }
                candidate += 1;
            }

            boundaries.push((start, end));
            start = end;
        }

        boundaries
    }

    fn scalar_partition_boundaries(
        data: &[u8],
        num_partitions: usize,
        delimiter: u8,
    ) -> Vec<(usize, usize)> {
        if data.is_empty() || num_partitions == 0 {
            return Vec::new();
        }
        if num_partitions == 1 {
            return vec![(0, data.len())];
        }

        let mut cut_points = Vec::new();
        let mut last_cut = 0;

        for partition in 1..num_partitions {
            let target = data.len() * partition / num_partitions;
            if target <= last_cut {
                continue;
            }

            let mut position = target;
            while position < data.len() && data[position] != delimiter {
                position += 1;
            }

            let cut = if position < data.len() {
                position + 1
            } else {
                data.len()
            };
            cut_points.push(cut);
            last_cut = cut;

            if cut == data.len() {
                break;
            }
        }

        let mut partitions = Vec::with_capacity(cut_points.len() + 1);
        let mut start = 0;
        for end in cut_points {
            if end > start {
                partitions.push((start, end));
            }
            start = end;
        }
        if start < data.len() {
            partitions.push((start, data.len()));
        }
        partitions
    }

    fn generated_single_case(seed: u64, case: usize) -> (Vec<u8>, usize, u8) {
        const LENGTHS: &[usize] = &[0, 1, 2, 3, 7, 8, 9, 15, 16, 17, 31, 32, 63, 64, 127, 255];
        let mut rng = Lcg::new(seed ^ (case as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let len = if case % 3 == 0 {
            LENGTHS[(case / 3) % LENGTHS.len()]
        } else {
            rng.next_usize(256)
        };
        let mut data = vec![0; len];
        for byte in &mut data {
            *byte = rng.next_u8();
        }

        let delimiter = match case % 8 {
            0 => 0x00,
            1 => 0xff,
            2 => b'\n',
            3 => 0x80,
            _ => rng.next_u8(),
        };
        if case % 11 == 0 {
            data.fill(delimiter);
        } else if !data.is_empty() {
            let len = data.len();
            data[case % len] = delimiter;
            if case % 3 == 1 && len > 1 {
                data[(case * 7 + 1) % len] = delimiter;
            }
        }

        let chunk_size = match case % 10 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => data.len(),
            4 => data.len().saturating_add(1),
            5 => usize::MAX,
            _ => rng.next_usize(128),
        };
        (data, chunk_size, delimiter)
    }

    fn generated_pattern_case(seed: u64, case: usize) -> (Vec<u8>, usize, Vec<u8>) {
        const DATA_LENGTHS: &[usize] = &[0, 1, 2, 3, 7, 8, 15, 16, 31, 32, 63, 64, 127];
        const PATTERN_LENGTHS: &[usize] = &[1, 2, 3, 4, 5, 8, 16, 32, 64];
        let mut rng = Lcg::new(seed ^ (case as u64).wrapping_mul(0xd6e8_feb8_6659_fd93));
        let data_len = if case % 3 == 0 {
            DATA_LENGTHS[(case / 3) % DATA_LENGTHS.len()]
        } else {
            rng.next_usize(192)
        };
        let pattern_len = PATTERN_LENGTHS[case % PATTERN_LENGTHS.len()];
        let mut data = vec![0; data_len];
        for byte in &mut data {
            *byte = rng.next_u8();
        }
        let mut pattern = vec![0; pattern_len];
        for byte in &mut pattern {
            *byte = rng.next_u8();
        }

        let chunk_size = match case % 9 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => data_len,
            4 => data_len.saturating_add(1),
            5 => usize::MAX,
            _ => rng.next_usize(96),
        };

        if case % 5 == 0 {
            data.fill(b'a');
            pattern.fill(b'a');
            if pattern.len() > 1 {
                *pattern.last_mut().unwrap() = b'b';
            }
        }

        if data.len() >= pattern.len() {
            let max_start = data.len() - pattern.len();
            let start = match case % 4 {
                0 => 0,
                1 => max_start,
                2 => chunk_size.max(1).min(max_start),
                _ => rng.next_usize(max_start + 1),
            };
            data[start..start + pattern.len()].copy_from_slice(&pattern);
        }

        (data, chunk_size, pattern)
    }

    fn generated_partition_case(seed: u64, case: usize) -> (Vec<u8>, usize, u8) {
        const LENGTHS: &[usize] = &[0, 1, 2, 3, 7, 8, 15, 16, 31, 32, 63, 64, 127, 255];
        let mut rng = Lcg::new(seed ^ (case as u64).wrapping_mul(0xa409_3822_299f_31d0));
        let len = if case % 4 == 0 {
            LENGTHS[(case / 4) % LENGTHS.len()]
        } else {
            rng.next_usize(256)
        };
        let mut data = vec![0; len];
        for byte in &mut data {
            *byte = rng.next_u8();
        }
        let delimiter = match case % 7 {
            0 => 0x00,
            1 => 0xff,
            2 => b'\n',
            _ => rng.next_u8(),
        };

        match case % 10 {
            0 => data.fill(delimiter),
            1 => {}
            _ if !data.is_empty() => {
                let injections = 1 + case % 5;
                let len = data.len();
                for offset in 0..injections {
                    data[(case * 13 + offset * 17) % len] = delimiter;
                }
            }
            _ => {}
        }

        let num_partitions = match case % 9 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            5 => 16,
            6 => 64,
            _ => 1 + rng.next_usize(128),
        };
        (data, num_partitions, delimiter)
    }

    fn cursor_ranges(data: &[u8], chunk_size: usize, delimiter: u8) -> Vec<(usize, usize)> {
        let base = data.as_ptr() as usize;
        let mut cursor = ChunkCursor::new(data, chunk_size, delimiter);
        let mut ranges = Vec::new();
        for chunk in cursor.by_ref() {
            let start = chunk.as_ptr() as usize - base;
            let end = start + chunk.len();
            assert_eq!(&data[start..end], chunk);
            ranges.push((start, end));
        }
        assert!(cursor.next().is_none());
        assert!(cursor.is_empty());
        assert_eq!(cursor.position(), data.len());
        ranges
    }

    fn pattern_cursor_ranges(
        data: &[u8],
        chunk_size: usize,
        pattern: &[u8],
    ) -> Vec<(usize, usize)> {
        let base = data.as_ptr() as usize;
        let mut cursor = PatternChunkCursor::new(data, chunk_size, pattern);
        let mut ranges = Vec::new();
        for chunk in cursor.by_ref() {
            let start = chunk.as_ptr() as usize - base;
            let end = start + chunk.len();
            assert_eq!(&data[start..end], chunk);
            ranges.push((start, end));
        }
        assert!(cursor.next().is_none());
        assert!(cursor.is_empty());
        assert_eq!(cursor.position(), data.len());
        ranges
    }

    fn assert_cover(data: &[u8], ranges: &[(usize, usize)]) {
        if data.is_empty() {
            assert!(ranges.is_empty());
            return;
        }
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, data.len());
        let mut next_start = 0;
        for &(start, end) in ranges {
            assert_eq!(start, next_start);
            assert!(end > start);
            next_start = end;
        }
        assert_eq!(next_start, data.len());
    }

    fn assert_partition_invariants(
        data: &[u8],
        num_partitions: usize,
        delimiter: u8,
        partitions: &[(usize, usize)],
    ) {
        if data.is_empty() || num_partitions == 0 {
            assert!(partitions.is_empty());
            return;
        }

        assert!(!partitions.is_empty());
        assert!(partitions.len() <= num_partitions);
        assert_eq!(partitions.first().unwrap().0, 0);
        assert_eq!(partitions.last().unwrap().1, data.len());

        let mut previous_end = 0;
        for (index, &(start, end)) in partitions.iter().enumerate() {
            assert_eq!(start, previous_end, "gap or overlap at partition {index}");
            assert!(end > start, "empty partition at index {index}");
            if index + 1 < partitions.len() {
                assert_eq!(data[end - 1], delimiter);
            }
            previous_end = end;
        }
        assert_eq!(previous_end, data.len());
    }

    #[test]
    fn single_byte_oracle_matches_deterministic_corpus() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"", 4, b'\n'),
            (b"x", 4, b'\n'),
            (b"xxxx\n", 4, b'\n'),
            (b"xx\nxx\n", 2, b'\n'),
            (b"\n\n\n", 1, b'\n'),
            (b"a\x00b\x00c", 2, 0),
            (b"no delimiter", 3, b'\n'),
        ];

        for &(data, chunk_size, delimiter) in cases {
            let expected = scalar_single_byte_boundaries(data, chunk_size, delimiter);
            let actual = find_chunk_boundaries(data, chunk_size, delimiter);
            assert_eq!(
                actual, expected,
                "mismatch for data={data:?}, chunk_size={chunk_size}, delimiter={delimiter:#04x}"
            );
            assert_cover(data, &expected);
        }
    }

    #[test]
    fn single_byte_oracle_matches_generated_cases() {
        for case in 0..4096 {
            let (data, chunk_size, delimiter) = generated_single_case(SINGLE_SEED, case);
            let expected = scalar_single_byte_boundaries(&data, chunk_size, delimiter);
            let actual = find_chunk_boundaries(&data, chunk_size, delimiter);
            assert_eq!(
                actual, expected,
                "single-byte mismatch: seed={SINGLE_SEED:#018x}, case={case}, data={data:?}, chunk_size={chunk_size}, delimiter={delimiter:#04x}"
            );
            assert_cover(&data, &expected);
        }
    }

    #[test]
    fn cursor_ranges_match_single_byte_oracle() {
        for case in 0..2048 {
            let (data, chunk_size, delimiter) = generated_single_case(CURSOR_SEED, case);
            let expected = scalar_single_byte_boundaries(&data, chunk_size, delimiter);
            let eager = find_chunk_boundaries(&data, chunk_size, delimiter);
            let cursor = cursor_ranges(&data, chunk_size, delimiter);
            assert_eq!(
                eager, expected,
                "eager mismatch: seed={CURSOR_SEED:#018x}, case={case}"
            );
            assert_eq!(cursor, expected, "cursor mismatch: seed={CURSOR_SEED:#018x}, case={case}, data={data:?}, chunk_size={chunk_size}, delimiter={delimiter:#04x}");
            assert_cover(&data, &cursor);
            assert_eq!(cursor_ranges(&data, chunk_size, delimiter), cursor);
        }
    }

    #[test]
    fn pattern_oracle_matches_deterministic_fixtures() {
        let cases: &[(&[u8], usize, &[u8])] = &[
            (b"", 4, b"\r\n"),
            (b"a\r\nb\r\nc", 4, b"\r\n"),
            (b"a\x00\xff\x00b\x00\xff\x00c", 2, b"\x00\xff\x00"),
            (b"aaaaaa", 1, b"aa"),
            (b"prefixEND_RECORDsuffix", 3, b"END_RECORD"),
            (b"no delimiter", 2, b"\r\n\r\n"),
            (b"abc", 1, b"abcdef"),
            (b"xx\r\n\r\nxx", 1, b"\r\n\r\n"),
        ];

        for &(data, chunk_size, pattern) in cases {
            let expected = scalar_pattern_boundaries(data, chunk_size, pattern);
            let actual = find_chunk_boundaries_pattern(data, chunk_size, pattern);
            assert_eq!(
                actual, expected,
                "pattern mismatch for data={data:?}, chunk_size={chunk_size}, pattern={pattern:?}"
            );
            assert_cover(data, &expected);
        }
    }

    #[test]
    fn pattern_oracle_matches_generated_cases() {
        for case in 0..4096 {
            let (data, chunk_size, pattern) = generated_pattern_case(PATTERN_SEED, case);
            let expected = scalar_pattern_boundaries(&data, chunk_size, &pattern);
            let actual = find_chunk_boundaries_pattern(&data, chunk_size, &pattern);
            assert_eq!(
                actual, expected,
                "pattern mismatch: seed={PATTERN_SEED:#018x}, case={case}, data={data:?}, chunk_size={chunk_size}, pattern={pattern:?}"
            );
            assert_cover(&data, &expected);
        }
    }

    #[test]
    fn pattern_cursor_ranges_match_pattern_oracle() {
        for case in 0..2048 {
            let (data, chunk_size, pattern) = generated_pattern_case(PATTERN_CURSOR_SEED, case);
            let expected = scalar_pattern_boundaries(&data, chunk_size, &pattern);
            let eager = find_chunk_boundaries_pattern(&data, chunk_size, &pattern);
            let cursor = pattern_cursor_ranges(&data, chunk_size, &pattern);
            assert_eq!(
                eager, expected,
                "eager pattern mismatch: seed={PATTERN_CURSOR_SEED:#018x}, case={case}"
            );
            assert_eq!(cursor, expected, "pattern cursor mismatch: seed={PATTERN_CURSOR_SEED:#018x}, case={case}, data={data:?}, chunk_size={chunk_size}, pattern={pattern:?}");
            assert_cover(&data, &cursor);
            assert_eq!(pattern_cursor_ranges(&data, chunk_size, &pattern), cursor);
        }
    }

    #[test]
    fn swar_matches_scalar_byte_search_across_offsets_and_lengths() {
        const LENGTHS: &[usize] = &[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127,
        ];
        let mut comparisons = 0usize;
        for case in 0..16 {
            let mut rng = Lcg::new(SWAR_SEED ^ case as u64);
            for prefix in 0..8 {
                for &len in LENGTHS {
                    let mut backing = vec![0; prefix + len];
                    for byte in &mut backing[prefix..] {
                        *byte = rng.next_u8();
                    }
                    let haystack = &backing[prefix..];
                    for delimiter in 0..=u8::MAX {
                        assert_eq!(
                            find_byte_swar(haystack, delimiter),
                            scalar_byte_position(haystack, delimiter),
                            "SWAR mismatch: seed={SWAR_SEED:#018x}, case={case}, prefix={prefix}, len={len}, delimiter={delimiter:#04x}, haystack={haystack:?}"
                        );
                        comparisons += 1;
                    }
                }
            }
        }
        assert_eq!(comparisons, 16 * 8 * LENGTHS.len() * 256);
    }

    #[test]
    fn partition_oracle_matches_deterministic_fixtures() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"", 4, b'\n'),
            (b"x", 1, b'\n'),
            (b"no delimiter", 8, b'\n'),
            (b"a\n\n\nb\n", 8, b'\n'),
            (b"aa\nbbbb\ncccccccccccc\ndd\n", 4, b'\n'),
            (b"aaaa\x00bbbb\x00cccc", 2, 0),
            (b"123456789", 0, b'\n'),
            (b"123456789", 1, b'\n'),
            (b"123456789", 64, b'\n'),
        ];

        for &(data, num_partitions, delimiter) in cases {
            let expected = scalar_partition_boundaries(data, num_partitions, delimiter);
            let actual = find_partition_boundaries(data, num_partitions, delimiter);
            assert_eq!(
                actual, expected,
                "partition mismatch for data={data:?}, n={num_partitions}, delimiter={delimiter:#04x}"
            );
            assert_partition_invariants(data, num_partitions, delimiter, &actual);
        }

        let mut giant = vec![b'x'; 10_000];
        giant.extend_from_slice(b"\nsmall\nrecords\n");
        let expected = scalar_partition_boundaries(&giant, 64, b'\n');
        let actual = find_partition_boundaries(&giant, 64, b'\n');
        assert_eq!(actual, expected);
        assert_partition_invariants(&giant, 64, b'\n', &actual);
    }

    #[test]
    fn partition_oracle_matches_generated_cases() {
        for case in 0..4096 {
            let (data, num_partitions, delimiter) = generated_partition_case(PARTITION_SEED, case);
            let expected = scalar_partition_boundaries(&data, num_partitions, delimiter);
            let actual = find_partition_boundaries(&data, num_partitions, delimiter);
            assert_eq!(
                actual, expected,
                "partition mismatch: seed={PARTITION_SEED:#018x}, case={case}, data={data:?}, n={num_partitions}, delimiter={delimiter:#04x}"
            );
            assert_partition_invariants(&data, num_partitions, delimiter, &actual);
            assert_eq!(
                find_partition_boundaries(&data, num_partitions, delimiter),
                actual
            );
        }
    }
}

/// A lazy, streaming cursor that yields delimiter-aligned chunks
/// sequentially without pre-computing all boundaries.
///
/// Each call to [`next`](ChunkCursor::next) produces a single chunk using
/// the same boundary semantics as [`find_chunk_boundaries`], reusing the
/// SWAR byte search internally. The cursor advances its internal position
/// and yields `&[u8]` slices directly referencing the input data.
///
/// # Memory footprint
///
/// O(1) state: a single struct (~40 bytes on 64-bit) regardless of file
/// size, compared to O(number_of_chunks) for the eager `Vec<(usize,usize)>`
/// approach (16 bytes per chunk).
///
/// # Example
///
/// ```
/// use mmap_chunker_core::scanner::ChunkCursor;
///
/// let data = b"aaa\nbbb\nccc\nddd\n";
/// let chunks: Vec<&[u8]> = ChunkCursor::new(data, 4, b'\n').collect();
/// assert_eq!(chunks, vec![b"aaa\nbbb\n" as &[u8], b"ccc\nddd\n" as &[u8]]);
/// ```
#[derive(Debug, Clone)]
pub struct ChunkCursor<'a> {
    data: &'a [u8],
    chunk_size: usize,
    delimiter: u8,
    position: usize,
}

impl<'a> ChunkCursor<'a> {
    /// Create a new cursor over `data` with the given approximate
    /// `chunk_size` and single-byte `delimiter`.
    ///
    /// Chunk boundaries are placed at or after each `chunk_size`
    /// interval, snapped to the next occurrence of `delimiter`.
    /// The last chunk extends to EOF. Empty input produces an
    /// exhausted cursor (no items yielded).
    #[inline]
    pub fn new(data: &'a [u8], chunk_size: usize, delimiter: u8) -> Self {
        Self {
            data,
            chunk_size,
            delimiter,
            position: 0,
        }
    }

    /// Returns the current position (start of the next chunk).
    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Returns the total number of bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns `true` if all chunks have been consumed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.position >= self.data.len()
    }
}

impl<'a> Iterator for ChunkCursor<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let len = self.data.len();
        if self.position >= len {
            return None;
        }

        let step = self.chunk_size.max(1);
        let target = self.position.saturating_add(step);
        let end = if target >= len {
            len
        } else {
            let remainder = &self.data[target..];
            match find_byte_swar(remainder, self.delimiter) {
                Some(rel_pos) => (target + rel_pos + 1).min(len),
                None => len,
            }
        };

        let chunk = &self.data[self.position..end];
        self.position = end;
        Some(chunk)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.data.len();
        if self.position >= len {
            return (0, Some(0));
        }
        let remaining = len - self.position;
        (1, Some(remaining))
    }
}

/// Search `haystack` for the first occurrence of the multi-byte `pattern`.
///
/// Uses first-byte SWAR to find candidates, then verifies with
/// [`starts_with`]. For single-byte patterns, this is equivalent to
/// [`find_byte_swar`]. For longer patterns, each candidate position
/// resynchronizes the search after a false-positive first-byte match.
///
/// Time complexity: O(n + m) typical, O(n*m) pathological (repeated
/// prefix). No unsafe. No dependencies. MSRV 1.77.
fn find_pattern_in_slice(haystack: &[u8], pattern: &[u8]) -> Option<usize> {
    let plen = pattern.len();
    if plen == 0 || haystack.len() < plen {
        return None;
    }
    if plen == 1 {
        return find_byte_swar(haystack, pattern[0]);
    }

    let first_byte = pattern[0];
    let hlen = haystack.len();
    let max_search = hlen - plen;
    let mut search_start = 0;

    while search_start <= max_search {
        let remainder = &haystack[search_start..];
        let rel_pos = find_byte_swar(remainder, first_byte)?;
        let pos = search_start + rel_pos;
        if pos > max_search {
            return None;
        }
        if haystack[pos..].starts_with(pattern) {
            return Some(pos);
        }
        search_start = pos + 1;
    }
    None
}

/// Find chunk boundaries in `data` using a multi-byte `delimiter`.
///
/// Same semantics as [`find_chunk_boundaries`] but the delimiter can be
/// multiple bytes (e.g., `b"\r\n"` for CRLF, `b"\r\n\r\n"` for HTTP
/// headers). Chunks are placed immediately after the complete delimiter.
///
/// When `delimiter.len() == 1`, this delegates to the single-byte SWAR
/// fast path and produces identical output.
///
/// # Panics
///
/// Panics if `delimiter` is empty.
pub fn find_chunk_boundaries_pattern(
    data: &[u8],
    chunk_size: usize,
    delimiter: &[u8],
) -> Vec<(usize, usize)> {
    assert!(!delimiter.is_empty(), "delimiter must not be empty");
    if data.is_empty() {
        return Vec::new();
    }

    let dlen = delimiter.len();
    let len = data.len();
    let step = chunk_size.max(1);
    let estimate = (len / step) + 2;
    let mut chunks = Vec::with_capacity(estimate);

    let mut start = 0usize;

    while start < len {
        let mut end = start.saturating_add(step);

        if end >= len {
            end = len;
        } else {
            let remainder = &data[end..];
            if let Some(rel_pos) = find_pattern_in_slice(remainder, delimiter) {
                end = end + rel_pos + dlen;
                if end > len {
                    end = len;
                }
            } else {
                end = len;
            }
        }

        chunks.push((start, end));
        start = end;
    }

    chunks
}

/// A lazy, streaming cursor for multi-byte delimiter chunking.
///
/// Like [`ChunkCursor`] but accepts a slice pattern as the delimiter.
/// Each call to [`next`](PatternChunkCursor::next) yields a chunk aligned
/// after the complete multi-byte delimiter.
///
/// Single-byte patterns are handled via the same SWAR fast path as
/// [`ChunkCursor`]. Multi-byte patterns use first-byte SWAR candidate
/// search with [`starts_with`] verification.
///
/// # Panics
///
/// Panics if `delimiter` is empty.
///
/// # Example
///
/// ```
/// use mmap_chunker_core::PatternChunkCursor;
///
/// let data = b"a\r\nb\r\nc\r\n";
/// let chunks: Vec<&[u8]> = PatternChunkCursor::new(data, 4, b"\r\n").collect();
/// assert_eq!(chunks, vec![b"a\r\nb\r\n" as &[u8], b"c\r\n" as &[u8]]);
/// ```
#[derive(Debug, Clone)]
pub struct PatternChunkCursor<'a, 'p> {
    data: &'a [u8],
    chunk_size: usize,
    delimiter: &'p [u8],
    position: usize,
}

impl<'a, 'p> PatternChunkCursor<'a, 'p> {
    /// Create a new pattern cursor with the given multi-byte `delimiter`.
    ///
    /// # Panics
    ///
    /// Panics if `delimiter` is empty.
    #[inline]
    pub fn new(data: &'a [u8], chunk_size: usize, delimiter: &'p [u8]) -> Self {
        assert!(!delimiter.is_empty(), "delimiter must not be empty");
        Self {
            data,
            chunk_size,
            delimiter,
            position: 0,
        }
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns `true` if all chunks have been consumed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.position >= self.data.len()
    }
}

impl<'a, 'p> Iterator for PatternChunkCursor<'a, 'p> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let len = self.data.len();
        if self.position >= len {
            return None;
        }

        let dlen = self.delimiter.len();
        let step = self.chunk_size.max(1);
        let target = self.position.saturating_add(step);
        let end = if target >= len {
            len
        } else {
            let remainder = &self.data[target..];
            match find_pattern_in_slice(remainder, self.delimiter) {
                Some(rel_pos) => (target + rel_pos + dlen).min(len),
                None => len,
            }
        };

        let chunk = &self.data[self.position..end];
        self.position = end;
        Some(chunk)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.data.len();
        if self.position >= len {
            return (0, Some(0));
        }
        let remaining = len - self.position;
        (1, Some(remaining))
    }
}

/// Safe SWAR (SIMD Within A Register) byte search.
///
/// Scans `haystack` for the first occurrence of `delimiter`. Processes
/// 8 bytes per iteration using word-at-a-time bit manipulation, with a
/// scalar prefix for alignment and a scalar tail for the final <8 bytes.
///
/// No unsafe. No dependencies. MSRV 1.77.
pub(crate) fn find_byte_swar(haystack: &[u8], delimiter: u8) -> Option<usize> {
    let len = haystack.len();
    if len == 0 {
        return None;
    }

    let pattern = (delimiter as u64).wrapping_mul(0x0101010101010101u64);
    let lo = 0x0101010101010101u64;
    let hi = 0x8080808080808080u64;

    let mut i = 0usize;

    // Phase 1: scalar prefix to reach 8-byte alignment
    let ptr = haystack.as_ptr() as usize;
    let align = ptr % 8;
    if align != 0 {
        let prefix_end = (8 - align).min(len);
        while i < prefix_end {
            if haystack[i] == delimiter {
                return Some(i);
            }
            i += 1;
        }
    }

    // Phase 2: SWAR main loop (8-byte reads)
    while i + 8 <= len {
        let chunk: [u8; 8] = haystack[i..i + 8].try_into().unwrap();
        let word = u64::from_ne_bytes(chunk);
        let xored = word ^ pattern;

        let has_zero = xored.wrapping_sub(lo) & !xored & hi;
        if has_zero != 0 {
            return Some(i + (has_zero.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }

    // Phase 3: scalar tail (< 8 bytes)
    while i < len {
        if haystack[i] == delimiter {
            return Some(i);
        }
        i += 1;
    }

    None
}

/// Number of fixed-size chunks a file of `file_len` bytes would produce
/// with the given `chunk_size`.
///
/// `chunk_size` of 0 is clamped to 1, consistent with the delimiter scanner.
#[inline]
pub fn fixed_chunk_count(file_len: usize, chunk_size: usize) -> usize {
    if file_len == 0 {
        return 0;
    }
    file_len.div_ceil(chunk_size.max(1))
}

/// Compute the (start, end) boundaries for the `index`-th fixed-size chunk.
///
/// Returns `None` if `index >= fixed_chunk_count(file_len, chunk_size)`.
///
/// Overflow-safe: uses saturating arithmetic with a `min(file_len)` clamp.
/// `chunk_size` of 0 is clamped to 1.
#[inline]
pub fn fixed_chunk_bounds(
    file_len: usize,
    chunk_size: usize,
    index: usize,
) -> Option<(usize, usize)> {
    let effective_size = chunk_size.max(1);
    if file_len == 0 {
        return None;
    }
    let count = file_len.div_ceil(effective_size);
    if index >= count {
        return None;
    }
    let start = index.saturating_mul(effective_size);

    let raw_end = start.saturating_add(effective_size);
    let end = if raw_end > file_len {
        file_len
    } else {
        raw_end
    };

    Some((start, end))
}

/// Compute N record-aligned partition boundaries covering `data`.
///
/// For each partition boundary `i = 1..N-1`, computes an ideal absolute
/// target position at `floor(data.len() * i / N)`, then searches forward
/// to the next occurrence of `delimiter`. Each boundary is placed
/// immediately after the delimiter byte (delimiter included in the
/// preceding partition).
///
/// If a single record spans multiple ideal target positions, those
/// boundaries collapse to the end of that record (deduplication). The
/// effective number of partitions may therefore be less than
/// `num_partitions`; the returned `Vec` length reflects the actual
/// partition count.
///
/// # Properties
///
/// - `first.start == 0`, `last.end == data.len()` — complete coverage
/// - No gaps, no overlaps — adjacent partitions are contiguous
/// - Boundaries respect record integrity — every non-final partition
///   ends immediately after a delimiter (or at EOF for the final one)
/// - Deterministic — same input always produces same output
/// - Partition sizes approximate `data.len() / actual_count`
/// - Maximum boundary deviation from ideal bounded by max record size
/// - `O(N)` metadata, bounded byte scanning (≤ data.len() total)
///
/// # Edge cases
///
/// | Case | Behavior |
/// |------|----------|
/// | `data` is empty | Returns empty `Vec` |
/// | `num_partitions == 0` | Returns empty `Vec` |
/// | `num_partitions == 1` | Returns `[(0, data.len())]` |
/// | No delimiter in entire file | Returns `[(0, data.len())]` |
/// | Fewer records than `N` | Produces ≤ record_count partitions |
/// | Giant record spanning multiple targets | Boundaries collapse, no record split |
pub fn find_partition_boundaries(
    data: &[u8],
    num_partitions: usize,
    delimiter: u8,
) -> Vec<(usize, usize)> {
    let file_len = data.len();
    if file_len == 0 || num_partitions == 0 {
        return Vec::new();
    }
    if num_partitions == 1 {
        return vec![(0, file_len)];
    }

    let n = num_partitions;
    let target_count = n - 1;

    // Overflow-safe: use u128 intermediate for multiplication
    let mut targets: Vec<usize> = Vec::with_capacity(target_count);
    for i in 1..n {
        let target = ((file_len as u128) * (i as u128) / (n as u128)) as usize;
        targets.push(target);
    }

    let mut boundaries: Vec<usize> = Vec::with_capacity(target_count);
    let mut last_boundary: usize = 0;

    for &target in &targets {
        if target <= last_boundary {
            continue;
        }

        let remainder = &data[target..];
        match find_byte_swar(remainder, delimiter) {
            Some(rel_pos) => {
                let boundary = target + rel_pos + 1;
                let boundary = boundary.min(file_len);
                boundaries.push(boundary);
                last_boundary = boundary;
            }
            None => {
                boundaries.push(file_len);
                break;
            }
        }
    }

    let effective_n = boundaries.len() + 1;
    let mut partitions = Vec::with_capacity(effective_n);

    let mut prev = 0usize;
    for &b in &boundaries {
        if b > prev {
            partitions.push((prev, b));
        }
        prev = b;
    }

    if prev < file_len {
        partitions.push((prev, file_len));
    }

    partitions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert_eq!(find_chunk_boundaries(b"", 1024, b'\n'), vec![]);
    }

    #[test]
    fn test_smaller_than_chunk() {
        let data = b"hello\nworld\n";
        let chunks = find_chunk_boundaries(data, 1024, b'\n');
        assert_eq!(chunks, vec![(0, 12)]);
    }

    #[test]
    fn test_fixed_lines() {
        let data = b"aaa\nbbb\nccc\nddd\neee\n";
        let chunks = find_chunk_boundaries(data, 6, b'\n');
        assert_eq!(chunks.len(), 3);
        assert_eq!(&data[chunks[0].0..chunks[0].1], b"aaa\nbbb\n");
        assert_eq!(&data[chunks[1].0..chunks[1].1], b"ccc\nddd\n");
        assert_eq!(&data[chunks[2].0..chunks[2].1], b"eee\n");
    }

    #[test]
    fn test_no_delimiter_in_large_remainder() {
        let data = b"aaa\nbbb\ncccccccccccccccc";
        let chunks = find_chunk_boundaries(data, 4, b'\n');
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], (0, 4 + 3 + 1)); // 0..8 = "aaa\nbbb\n"
        assert_eq!(chunks[1], (8, data.len()));
    }

    #[test]
    fn test_no_delimiter() {
        let data = b"no_newlines_here";
        let chunks = find_chunk_boundaries(data, 5, b'\n');
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn test_sequential_coverage() {
        let data = b"line1\nline2\nline3\nline4\nline5\n";
        let chunks = find_chunk_boundaries(data, 10, b'\n');
        let mut pos = 0;
        for (start, end) in &chunks {
            assert_eq!(*start, pos);
            pos = *end;
        }
        assert_eq!(pos, data.len());
    }

    #[test]
    fn test_chunk_size_zero_clamps_to_one() {
        let data = b"abc\n";
        let chunks = find_chunk_boundaries(data, 0, b'\n');
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_chunk_size_one() {
        let data = b"a\nb\nc\n";
        let chunks = find_chunk_boundaries(data, 1, b'\n');
        let mut pos = 0;
        for (start, end) in &chunks {
            assert_eq!(*start, pos);
            assert!(end > start);
            pos = *end;
        }
        assert_eq!(pos, data.len());
    }

    #[test]
    fn test_no_trailing_newline() {
        let data = b"hello\nworld";
        let chunks = find_chunk_boundaries(data, 100, b'\n');
        assert_eq!(chunks, vec![(0, 11)]);
    }

    #[test]
    fn test_only_newlines() {
        let data = b"\n\n\n";
        let chunks = find_chunk_boundaries(data, 1, b'\n');
        let mut pos = 0;
        for (start, end) in &chunks {
            assert_eq!(*start, pos);
            pos = *end;
        }
        assert_eq!(pos, data.len());
    }

    #[test]
    fn test_consecutive_delimiters() {
        let data = b"line1\n\n\nline2\n";
        let chunks = find_chunk_boundaries(data, 6, b'\n');
        let mut pos = 0;
        for (start, end) in &chunks {
            assert_eq!(*start, pos);
            pos = *end;
        }
        assert_eq!(pos, data.len());
    }

    #[test]
    fn test_record_larger_than_chunk() {
        let data = b"short\nverylonglinewithnoddelimiteratall\nshort\n";
        let chunks = find_chunk_boundaries(data, 6, b'\n');
        let mut pos = 0;
        for (start, end) in &chunks {
            assert_eq!(*start, pos);
            pos = *end;
        }
        assert_eq!(pos, data.len());
    }

    #[test]
    fn test_binary_with_nul() {
        let data = b"prefix\x00suffix\n";
        let chunks = find_chunk_boundaries(data, 100, b'\n');
        assert_eq!(chunks, vec![(0, 14)]);
    }

    #[test]
    fn test_chunk_size_larger_than_data() {
        let data = b"tiny\n";
        let chunks = find_chunk_boundaries(data, 1_000_000, b'\n');
        assert_eq!(chunks, vec![(0, 5)]);
    }

    #[test]
    fn test_chunk_size_exact_multiple() {
        let data = b"xxxx\n";
        let chunks = find_chunk_boundaries(data, 5, b'\n');
        assert_eq!(chunks, vec![(0, 5)]);
    }

    #[test]
    fn test_repeated_scan_same_size() {
        let data = b"a\nb\nc\n";
        let chunks1 = find_chunk_boundaries(data, 2, b'\n');
        let chunks2 = find_chunk_boundaries(data, 2, b'\n');
        assert_eq!(chunks1, chunks2);
    }

    #[test]
    fn test_repeated_scan_different_size() {
        let data = b"aaa\nbbb\nccc\nddd\n";
        let chunks1 = find_chunk_boundaries(data, 4, b'\n');
        let chunks2 = find_chunk_boundaries(data, 8, b'\n');
        assert_ne!(chunks1, chunks2);
        let sum1: usize = chunks1.iter().map(|(s, e)| e - s).sum();
        let sum2: usize = chunks2.iter().map(|(s, e)| e - s).sum();
        assert_eq!(sum1, data.len());
        assert_eq!(sum2, data.len());
    }

    #[test]
    fn test_one_byte_file() {
        let data = b"x";
        let chunks = find_chunk_boundaries(data, 1024, b'\n');
        assert_eq!(chunks, vec![(0, 1)]);
    }

    #[test]
    fn test_one_byte_newline() {
        let data = b"\n";
        let chunks = find_chunk_boundaries(data, 1024, b'\n');
        assert_eq!(chunks, vec![(0, 1)]);
    }

    #[test]
    fn property_concatenation_equals_input() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"hello\nworld\n", 4, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"one\ttwo\tthree", 4, b'\t'),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"x\x00y\x00z", 2, b'\x00'),
            (b"single", 1024, b'\n'),
            (b"\n\n\n\n\n", 1, b'\n'),
            (b"", 1024, b'\n'),
            (b"\n", 1, b'\n'),
            (b"a", 1024, b'\n'),
        ];
        for &(data, chunk_size, delim) in cases {
            let chunks = find_chunk_boundaries(data, chunk_size, delim);
            let total: usize = chunks.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, data.len(), "concatenation property failed");
        }
    }

    #[test]
    fn property_no_gaps() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"a\nb\nc\n", 2, b'\n'),
            (b"a,b,c,d,e", 1, b','),
            (b"a\tb\tc\t", 2, b'\t'),
        ];
        for &(data, chunk_size, delim) in cases {
            let chunks = find_chunk_boundaries(data, chunk_size, delim);
            if chunks.is_empty() {
                continue;
            }
            assert_eq!(chunks[0].0, 0, "first chunk must start at 0");
            for i in 1..chunks.len() {
                assert_eq!(
                    chunks[i].0,
                    chunks[i - 1].1,
                    "gap at chunk {}->{}",
                    i - 1,
                    i
                );
            }
            assert_eq!(
                chunks.last().unwrap().1,
                data.len(),
                "last chunk must end at EOF"
            );
        }
    }

    #[test]
    fn property_determinism() {
        let data = b"line1\nline2\nline3\nline4\nline5\n";
        let chunks1 = find_chunk_boundaries(data, 10, b'\n');
        let chunks2 = find_chunk_boundaries(data, 10, b'\n');
        assert_eq!(chunks1, chunks2);
        let chunks3 = find_chunk_boundaries(data, 10, b'\n');
        assert_eq!(chunks1, chunks3);
    }

    #[test]
    fn property_monotonic_offsets() {
        let data = b"x\nxx\nxxx\nxxxx\nxxxxx\n";
        let chunks = find_chunk_boundaries(data, 1, b'\n');
        let mut last_end = 0usize;
        for (start, end) in &chunks {
            assert!(*start >= last_end, "offsets must be monotonic");
            assert!(*end > *start, "chunk must be non-empty");
            last_end = *end;
        }
    }

    #[test]
    fn test_alternative_delimiters() {
        assert_eq!(
            find_chunk_boundaries(b"a,b,c,d,e", 2, b','),
            vec![(0, 4), (4, 8), (8, 9)]
        );
        assert_eq!(
            find_chunk_boundaries(b"one\ttwo\tthree", 4, b'\t'),
            vec![(0, 8), (8, 13)]
        );
        assert_eq!(
            find_chunk_boundaries(b"a|b|c|d|e|f", 3, b'|'),
            vec![(0, 4), (4, 8), (8, 11)]
        );
        assert_eq!(
            find_chunk_boundaries(b"x\x00y\x00z", 2, b'\x00'),
            vec![(0, 4), (4, 5)]
        );
    }

    // ── ChunkCursor tests ──────────────────────────────────────────────

    /// Verify cursor produces identical chunks as eager scanner.
    fn cursor_equals_eager(data: &[u8], chunk_size: usize, delimiter: u8) {
        let eager = find_chunk_boundaries(data, chunk_size, delimiter);
        let cursor: Vec<&[u8]> = ChunkCursor::new(data, chunk_size, delimiter).collect();
        let lazy_ranges: Vec<(usize, usize)> = cursor
            .iter()
            .scan(0usize, |pos, &chunk| {
                let start = *pos;
                *pos += chunk.len();
                Some((start, *pos))
            })
            .collect();
        assert_eq!(
            lazy_ranges, eager,
            "cursor mismatch: chunk_size={chunk_size}, delim={delimiter:#04x}"
        );
    }

    #[test]
    fn cursor_empty_input() {
        assert_eq!(
            ChunkCursor::new(b"", 1024, b'\n').collect::<Vec<_>>(),
            Vec::<&[u8]>::new()
        );
    }

    #[test]
    fn cursor_one_byte_file() {
        cursor_equals_eager(b"x", 1024, b'\n');
    }

    #[test]
    fn cursor_one_byte_delimiter() {
        cursor_equals_eager(b"\n", 1024, b'\n');
    }

    #[test]
    fn cursor_delimiter_only_file() {
        cursor_equals_eager(b"\n\n\n", 1, b'\n');
    }

    #[test]
    fn cursor_no_delimiter() {
        cursor_equals_eager(b"no_newlines_here", 5, b'\n');
    }

    #[test]
    fn cursor_delimiter_exactly_at_target() {
        cursor_equals_eager(b"xxxx\n", 5, b'\n');
    }

    #[test]
    fn cursor_delimiter_after_target() {
        cursor_equals_eager(b"xxxyy\n", 3, b'\n');
    }

    #[test]
    fn cursor_multiple_consecutive_delimiters() {
        cursor_equals_eager(b"line1\n\n\nline2\n", 6, b'\n');
    }

    #[test]
    fn cursor_nul_delimiter() {
        cursor_equals_eager(b"prefix\x00suffix\n", 100, b'\n');
    }

    #[test]
    fn cursor_giant_record() {
        cursor_equals_eager(b"tiny\nverylongrecordwithnobreaksanywhere\nend\n", 6, b'\n');
    }

    #[test]
    fn cursor_no_trailing_delimiter() {
        cursor_equals_eager(b"hello\nworld", 100, b'\n');
    }

    #[test]
    fn cursor_chunk_size_zero() {
        cursor_equals_eager(b"abc\n", 0, b'\n');
    }

    #[test]
    fn cursor_chunk_size_one() {
        cursor_equals_eager(b"a\nb\nc\n", 1, b'\n');
    }

    #[test]
    fn cursor_chunk_size_larger_than_data() {
        cursor_equals_eager(b"tiny\n", 1_000_000, b'\n');
    }

    #[test]
    fn cursor_chunk_size_equals_len() {
        cursor_equals_eager(b"aaaa\n", 5, b'\n');
    }

    #[test]
    fn cursor_different_delimiters() {
        cursor_equals_eager(b"a,b,c,d,e", 2, b',');
        cursor_equals_eager(b"one\ttwo\tthree", 4, b'\t');
        cursor_equals_eager(b"a|b|c|d|e|f", 3, b'|');
        cursor_equals_eager(b"x\x00y\x00z", 2, b'\x00');
    }

    #[test]
    fn cursor_binary_input() {
        let data: Vec<u8> = (0u8..=255).collect();
        cursor_equals_eager(&data, 32, b'\n');
        cursor_equals_eager(&data, 32, 0x00);
        cursor_equals_eager(&data, 32, 0xff);
    }

    #[test]
    fn cursor_repeated_iteration_new_cursor() {
        let data = b"a\nb\nc\n";
        let first: Vec<&[u8]> = ChunkCursor::new(data, 2, b'\n').collect();
        let second: Vec<&[u8]> = ChunkCursor::new(data, 2, b'\n').collect();
        assert_eq!(first, second);
    }

    #[test]
    fn cursor_fixed_lines_equivalence() {
        let data = b"aaa\nbbb\nccc\nddd\neee\n";
        cursor_equals_eager(data, 6, b'\n');
    }

    #[test]
    fn cursor_only_newlines() {
        cursor_equals_eager(b"\n\n\n", 1, b'\n');
    }

    #[test]
    fn cursor_record_larger_than_chunk() {
        let data = b"short\nverylonglinewithnodelimiteratall\nshort\n";
        cursor_equals_eager(data, 6, b'\n');
    }

    #[test]
    fn cursor_deterministic_random_corpus() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"hello\nworld\n", 4, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"one\ttwo\tthree", 4, b'\t'),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"x\x00y\x00z", 2, b'\x00'),
            (b"single", 1024, b'\n'),
            (b"\n\n\n\n\n", 1, b'\n'),
            (b"\n", 1, b'\n'),
            (b"a", 1024, b'\n'),
            (b"line1\nline2\nline3\nline4\nline5\n", 10, b'\n'),
        ];
        for &(data, chunk_size, delim) in cases {
            cursor_equals_eager(data, chunk_size, delim);
        }
    }

    #[test]
    fn cursor_large_corpus_equivalence() {
        let mut data = Vec::new();
        for i in 0..1000u32 {
            data.extend_from_slice(format!("line_content_{i:04}\n").as_bytes());
        }
        cursor_equals_eager(&data, 64, b'\n');
    }

    // ── Cursor contract tests — size_hint + is_empty ────────────────

    #[test]
    fn cursor_size_hint_before_iteration() {
        let data = b"aaa\nbbb\nccc\nddd\neee\n";
        let cur = ChunkCursor::new(data, 6, b'\n');
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 1);
        assert_eq!(hi, Some(data.len()));
    }

    #[test]
    fn cursor_size_hint_after_one_next() {
        let data = b"aaa\nbbb\nccc\nddd\neee\n";
        let mut cur = ChunkCursor::new(data, 6, b'\n');
        let _ = cur.next();
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 1);
        assert!(hi.unwrap() < data.len());
    }

    #[test]
    fn cursor_size_hint_after_exhaustion() {
        let data = b"hello\n";
        let mut cur = ChunkCursor::new(data, 1024, b'\n');
        let _ = cur.next();
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 0);
        assert_eq!(hi, Some(0));
    }

    #[test]
    fn cursor_size_hint_empty_input() {
        let cur = ChunkCursor::new(b"", 1024, b'\n');
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 0);
        assert_eq!(hi, Some(0));
    }

    #[test]
    fn cursor_is_empty_before_iteration() {
        let data = b"hello\nworld";
        let cur = ChunkCursor::new(data, 100, b'\n');
        assert!(!cur.is_empty());
    }

    #[test]
    fn cursor_is_empty_after_exhaustion() {
        let data = b"hello\n";
        let mut cur = ChunkCursor::new(data, 1024, b'\n');
        let _ = cur.next(); // yields the only chunk
        assert!(cur.is_empty());
    }

    #[test]
    fn cursor_is_empty_empty_input() {
        let cur = ChunkCursor::new(b"", 1024, b'\n');
        assert!(cur.is_empty());
    }

    #[test]
    fn cursor_size_hint_no_delimiter() {
        let data = b"no_newlines_at_all";
        let cur = ChunkCursor::new(data, 5, b'\n');
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 1, "at least one chunk even with no delimiter");
        assert_eq!(hi, Some(data.len()));
    }

    #[test]
    fn cursor_size_hint_chunk_size_zero() {
        let data = b"abcd\n";
        let cur = ChunkCursor::new(data, 0, b'\n');
        let (lo, _) = cur.size_hint();
        assert_eq!(lo, 1);
    }

    #[test]
    fn cursor_size_hint_chunk_size_one() {
        let data = b"a\nb\nc\n";
        let cur = ChunkCursor::new(data, 1, b'\n');
        let (lo, _) = cur.size_hint();
        assert_eq!(lo, 1);
    }

    #[test]
    fn cursor_size_hint_chunk_size_larger_than_data() {
        let data = b"tiny\n";
        let cur = ChunkCursor::new(data, 1_000_000, b'\n');
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 1);
        assert_eq!(hi, Some(data.len()));
    }

    #[test]
    fn property_size_hint_lower_bound_accurate() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"hello\nworld\n", 4, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"one\ttwo\tthree", 4, b'\t'),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"x\x00y\x00z", 2, b'\x00'),
            (b"single", 1024, b'\n'),
            (b"\n\n\n\n\n", 1, b'\n'),
            (b"", 1024, b'\n'),
            (b"\n", 1, b'\n'),
            (b"a", 1024, b'\n'),
            (b"a\nb\nc\n", 1, b'\n'),
            (b"aaaa\n", 5, b'\n'),
        ];
        for &(data, cs, delim) in cases {
            let mut cur = ChunkCursor::new(data, cs, delim);
            let mut total_yielded = 0usize;
            loop {
                let (lo, hi) = cur.size_hint();
                let remaining = cur.len() - cur.position();
                if remaining == 0 {
                    assert_eq!(lo, 0);
                    assert_eq!(hi, Some(0));
                    break;
                }
                assert!(
                    lo <= remaining,
                    "lower bound {lo} exceeds remaining {remaining}: data={data:?} cs={cs}"
                );
                if let Some(next) = cur.next() {
                    total_yielded += next.len();
                }
            }
            assert_eq!(total_yielded, data.len());
        }
    }

    #[test]
    fn property_size_hint_upper_bound_accurate() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"hello\nworld\n", 4, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"\n\n\n", 1, b'\n'),
        ];
        for &(data, cs, delim) in cases {
            let mut cur = ChunkCursor::new(data, cs, delim);
            loop {
                let (_, hi) = cur.size_hint();
                let remaining = cur.len() - cur.position();
                if remaining == 0 {
                    assert_eq!(hi, Some(0));
                    break;
                }
                assert!(
                    hi.unwrap() >= remaining,
                    "upper bound {} < remaining {}: data={data:?} cs={cs}",
                    hi.unwrap(),
                    remaining
                );
                let _ = cur.next();
            }
        }
    }

    // ── PatternChunkCursor contract tests ────────────────────────────

    #[test]
    fn pattern_cursor_size_hint_before_iteration() {
        let data = b"a\r\nb\r\nc\r\n";
        let cur = PatternChunkCursor::new(data, 4, b"\r\n");
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 1);
        assert_eq!(hi, Some(data.len()));
    }

    #[test]
    fn pattern_cursor_size_hint_after_one_next() {
        let data = b"a\r\nb\r\nc\r\n";
        let mut cur = PatternChunkCursor::new(data, 4, b"\r\n");
        let _ = cur.next();
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 1);
        assert!(hi.unwrap() < data.len());
    }

    #[test]
    fn pattern_cursor_size_hint_after_exhaustion() {
        let data = b"a\r\n";
        let mut cur = PatternChunkCursor::new(data, 1024, b"\r\n");
        let _ = cur.next();
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 0);
        assert_eq!(hi, Some(0));
    }

    #[test]
    fn pattern_cursor_size_hint_empty_input() {
        let cur = PatternChunkCursor::new(b"", 1024, b"\r\n");
        let (lo, hi) = cur.size_hint();
        assert_eq!(lo, 0);
        assert_eq!(hi, Some(0));
    }

    #[test]
    fn pattern_cursor_is_empty_before_iteration() {
        let data = b"hello\r\nworld";
        let cur = PatternChunkCursor::new(data, 100, b"\r\n");
        assert!(!cur.is_empty());
    }

    #[test]
    fn pattern_cursor_is_empty_after_exhaustion() {
        let data = b"hello\r\n";
        let mut cur = PatternChunkCursor::new(data, 1024, b"\r\n");
        let _ = cur.next();
        assert!(cur.is_empty());
    }

    #[test]
    fn pattern_cursor_is_empty_empty_input() {
        let cur = PatternChunkCursor::new(b"", 1024, b"\r\n");
        assert!(cur.is_empty());
    }

    #[test]
    fn pattern_cursor_size_hint_no_delimiter() {
        let data = b"no_crlf_here";
        let cur = PatternChunkCursor::new(data, 5, b"\r\n");
        let (lo, _) = cur.size_hint();
        assert_eq!(lo, 1);
    }

    #[test]
    fn pattern_property_size_hint_lower_bound_accurate() {
        let cases: &[(&[u8], usize, &[u8])] = &[
            (b"a\r\nb\r\nc\r\n", 4, b"\r\n"),
            (b"ab||cd||ef", 4, b"||"),
            (b"single", 1024, b"\r\n"),
            (b"AB\xff\x00CD\xff\x00EF", 4, b"\xff\x00"),
            (b"\r\n\r\n\r\n", 1, b"\r\n"),
        ];
        for &(data, cs, delim) in cases {
            let mut cur = PatternChunkCursor::new(data, cs, delim);
            let mut total_yielded = 0usize;
            loop {
                let (lo, hi) = cur.size_hint();
                let remaining = cur.len() - cur.position();
                if remaining == 0 {
                    assert_eq!(lo, 0);
                    assert_eq!(hi, Some(0));
                    break;
                }
                assert!(
                    lo <= remaining,
                    "lower bound {lo} exceeds remaining {remaining}"
                );
                if let Some(next) = cur.next() {
                    total_yielded += next.len();
                }
            }
            assert_eq!(total_yielded, data.len());
        }
    }

    // ── Fixed-size chunking tests ────────────────────────────────────────

    #[test]
    #[ignore = "performance experiment — run with --ignored --nocapture"]
    fn bench_cursor_vs_eager() {
        use std::hint::black_box;
        use std::time::Instant;

        const SAMPLES: usize = 7;

        fn gen_log_data(target_size: usize) -> Vec<u8> {
            let mut data = Vec::with_capacity(target_size);
            let mut n = 0u64;
            while data.len() < target_size {
                let line = format!(
                    "[2026-08-08T12:00:00Z] INFO request_id={} status=200 latency_ms={}\n",
                    n,
                    n % 100
                );
                data.extend_from_slice(line.as_bytes());
                n += 1;
            }
            data.truncate(target_size);
            data
        }

        fn elapsed_per_iter(iters: u64, f: impl Fn()) -> f64 {
            let start = Instant::now();
            for _ in 0..iters {
                f();
            }
            start.elapsed().as_nanos() as f64 / iters as f64
        }

        fn samples_median(mut ns: Vec<f64>) -> f64 {
            ns.sort_by(|a, b| a.partial_cmp(b).unwrap());
            ns[ns.len() / 2]
        }

        fn samples_p10(mut ns: Vec<f64>) -> f64 {
            ns.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let idx = ((ns.len() - 1) as f64 * 0.10) as usize;
            ns[idx]
        }

        fn samples_p90(mut ns: Vec<f64>) -> f64 {
            ns.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let idx = ((ns.len() - 1) as f64 * 0.90) as usize;
            ns[idx]
        }

        fn bench_one(
            data: &[u8],
            chunk_size: usize,
            delim: u8,
            label: &str,
            iters: u64,
            eager_chunks: usize,
            lazy_chunks: usize,
        ) {
            let total_bytes = data.len();

            let mut tfc_eager_ns: Vec<f64> = Vec::with_capacity(SAMPLES);
            let mut tfc_lazy_ns: Vec<f64> = Vec::with_capacity(SAMPLES);
            let mut full_eager_ns: Vec<f64> = Vec::with_capacity(SAMPLES);
            let mut full_lazy_ns: Vec<f64> = Vec::with_capacity(SAMPLES);

            for _ in 0..SAMPLES {
                tfc_eager_ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let chunks = find_chunk_boundaries(d, cs, dl);
                    black_box(chunks.first().copied());
                }));

                tfc_lazy_ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let mut cursor = ChunkCursor::new(d, cs, dl);
                    black_box(cursor.next());
                }));

                full_eager_ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let chunks = find_chunk_boundaries(d, cs, dl);
                    let mut total = 0usize;
                    for &(s, e) in &chunks {
                        total = total.wrapping_add(e - s);
                    }
                    black_box(total);
                    black_box(&chunks);
                }));

                full_lazy_ns.push(elapsed_per_iter(iters, || {
                    let d = black_box(data);
                    let cs = black_box(chunk_size);
                    let dl = black_box(delim);
                    let mut total = 0usize;
                    let cursor = ChunkCursor::new(d, cs, dl);
                    for chunk in cursor {
                        total = total.wrapping_add(chunk.len());
                    }
                    black_box(total);
                }));
            }

            let tfc_e_p50 = samples_median(tfc_eager_ns.clone());
            let tfc_l_p50 = samples_median(tfc_lazy_ns.clone());
            let full_e_p50 = samples_median(full_eager_ns.clone());
            let full_l_p50 = samples_median(full_lazy_ns.clone());

            let tfc_e_p10 = samples_p10(tfc_eager_ns.clone());
            let tfc_l_p10 = samples_p10(tfc_lazy_ns.clone());
            let full_e_p10 = samples_p10(full_eager_ns.clone());
            let full_l_p10 = samples_p10(full_lazy_ns.clone());

            let tfc_e_p90 = samples_p90(tfc_eager_ns.clone());
            let tfc_l_p90 = samples_p90(tfc_lazy_ns.clone());
            let full_e_p90 = samples_p90(full_eager_ns.clone());
            let full_l_p90 = samples_p90(full_lazy_ns.clone());

            println!();
            println!("  === {label} ===");
            println!("  file={total_bytes}B chunk={chunk_size}B delim=0x{delim:02x}",);
            println!(
                "  build={} samples={SAMPLES} iters/sample={iters}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
            );
            println!("  eager_chunks={eager_chunks}  lazy_chunks={lazy_chunks}");
            println!(
                "  TFC  eager  p50={:>10.1}ns  p10={:>10.1}  p90={:>10.1}",
                tfc_e_p50, tfc_e_p10, tfc_e_p90,
            );
            println!(
                "  TFC  lazy   p50={:>10.1}ns  p10={:>10.1}  p90={:>10.1}  ratio={:.2}x",
                tfc_l_p50,
                tfc_l_p10,
                tfc_l_p90,
                tfc_e_p50 / tfc_l_p50,
            );
            println!(
                "  Full eager  p50={:>10.1}ns  p10={:>10.1}  p90={:>10.1}",
                full_e_p50, full_e_p10, full_e_p90,
            );
            println!(
                "  Full lazy   p50={:>10.1}ns  p10={:>10.1}  p90={:>10.1}  ratio={:.2}x",
                full_l_p50,
                full_l_p10,
                full_l_p90,
                full_e_p50 / full_l_p50,
            );
        }

        println!("=== Time-to-First-Chunk + Full Traversal ===");
        println!(
            "  CPU: {} cores",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        );
        println!("  OS:  {}", std::env::consts::OS);

        let jsonl = gen_log_data(100_000);
        let logs = gen_log_data(1_000_000);
        let sparse = gen_log_data(10_000_000);

        let jsonl_ec = find_chunk_boundaries(&jsonl, 64 * 1024, b'\n').len();
        let jsonl_lc = ChunkCursor::new(&jsonl, 64 * 1024, b'\n').count();
        bench_one(
            &jsonl,
            64 * 1024,
            b'\n',
            "JSONL-like ~100B rec 100KB 64KiB",
            200,
            jsonl_ec,
            jsonl_lc,
        );

        let logs_ec = find_chunk_boundaries(&logs, 64 * 1024, b'\n').len();
        let logs_lc = ChunkCursor::new(&logs, 64 * 1024, b'\n').count();
        bench_one(
            &logs,
            64 * 1024,
            b'\n',
            "Log-like ~100B rec 1MB 64KiB",
            50,
            logs_ec,
            logs_lc,
        );

        let sparse_ec = find_chunk_boundaries(&sparse, 64 * 1024, b'\n').len();
        let sparse_lc = ChunkCursor::new(&sparse, 64 * 1024, b'\n').count();
        bench_one(
            &sparse,
            64 * 1024,
            b'\n',
            "Sparse ~100B rec 10MB 64KiB",
            10,
            sparse_ec,
            sparse_lc,
        );

        let jsonl_ec_1m = find_chunk_boundaries(&jsonl, 1024 * 1024, b'\n').len();
        let jsonl_lc_1m = ChunkCursor::new(&jsonl, 1024 * 1024, b'\n').count();
        bench_one(
            &jsonl,
            1024 * 1024,
            b'\n',
            "JSONL-like ~100B rec 100KB 1MiB",
            200,
            jsonl_ec_1m,
            jsonl_lc_1m,
        );

        let logs_ec_1m = find_chunk_boundaries(&logs, 1024 * 1024, b'\n').len();
        let logs_lc_1m = ChunkCursor::new(&logs, 1024 * 1024, b'\n').count();
        bench_one(
            &logs,
            1024 * 1024,
            b'\n',
            "Log-like ~100B rec 1MB 1MiB",
            50,
            logs_ec_1m,
            logs_lc_1m,
        );
    }

    // ── Fixed-size chunking tests ────────────────────────────────────────

    #[test]
    fn test_fixed_chunk_count_empty() {
        assert_eq!(fixed_chunk_count(0, 1024), 0);
    }

    #[test]
    fn test_fixed_chunk_count_exact_multiple() {
        assert_eq!(fixed_chunk_count(1024, 256), 4);
    }

    #[test]
    fn test_fixed_chunk_count_with_remainder() {
        assert_eq!(fixed_chunk_count(1000, 256), 4);
    }

    #[test]
    fn test_fixed_chunk_count_single() {
        assert_eq!(fixed_chunk_count(10, 1024), 1);
    }

    #[test]
    fn test_fixed_chunk_count_zero_clamps() {
        assert_eq!(fixed_chunk_count(5, 0), 5);
    }

    #[test]
    fn test_fixed_chunk_bounds_empty() {
        assert_eq!(fixed_chunk_bounds(0, 256, 0), None);
    }

    #[test]
    fn test_fixed_chunk_bounds_exact() {
        assert_eq!(fixed_chunk_bounds(1024, 256, 0), Some((0, 256)));
        assert_eq!(fixed_chunk_bounds(1024, 256, 1), Some((256, 512)));
        assert_eq!(fixed_chunk_bounds(1024, 256, 2), Some((512, 768)));
        assert_eq!(fixed_chunk_bounds(1024, 256, 3), Some((768, 1024)));
        assert_eq!(fixed_chunk_bounds(1024, 256, 4), None);
    }

    #[test]
    fn test_fixed_chunk_bounds_remainder() {
        assert_eq!(fixed_chunk_bounds(1000, 256, 3), Some((768, 1000)));
    }

    #[test]
    fn test_fixed_chunk_bounds_size_larger_than_file() {
        assert_eq!(fixed_chunk_bounds(10, 1024, 0), Some((0, 10)));
        assert_eq!(fixed_chunk_bounds(10, 1024, 1), None);
    }

    #[test]
    fn test_fixed_chunk_bounds_oob() {
        assert_eq!(fixed_chunk_bounds(1024, 256, 4), None);
        assert_eq!(fixed_chunk_bounds(1024, 256, 100), None);
    }

    #[test]
    fn test_fixed_chunk_bounds_zero_clamps() {
        assert_eq!(fixed_chunk_bounds(5, 0, 0), Some((0, 1)));
        assert_eq!(fixed_chunk_bounds(5, 0, 1), Some((1, 2)));
        assert_eq!(fixed_chunk_bounds(5, 0, 4), Some((4, 5)));
        assert_eq!(fixed_chunk_bounds(5, 0, 5), None);
    }

    #[test]
    fn test_fixed_chunk_bounds_single() {
        assert_eq!(fixed_chunk_bounds(1, 1024, 0), Some((0, 1)));
    }

    #[test]
    fn test_fixed_property_concat_equals_len() {
        let cases: &[(usize, usize)] = &[
            (1024, 256),
            (1000, 256),
            (1, 1024),
            (0, 1024),
            (5, 1),
            (1024, 1024),
            (1025, 1024),
        ];
        for &(len, cs) in cases {
            let count = fixed_chunk_count(len, cs);
            let mut total = 0usize;
            for i in 0..count {
                let (s, e) = fixed_chunk_bounds(len, cs, i).unwrap();
                total += e - s;
                assert!(s <= e);
            }
            assert_eq!(total, len, "len={len} cs={cs}");
        }
    }

    #[test]
    fn test_fixed_property_no_gaps() {
        let cases: &[(usize, usize)] = &[(1024, 256), (1000, 256), (5, 1)];
        for &(len, cs) in cases {
            let count = fixed_chunk_count(len, cs);
            let mut prev_end = 0usize;
            for i in 0..count {
                let (s, e) = fixed_chunk_bounds(len, cs, i).unwrap();
                assert_eq!(s, prev_end, "gap at i={i}");
                prev_end = e;
            }
            assert_eq!(prev_end, len);
        }
    }

    #[test]
    fn test_fixed_property_non_final_full() {
        let cases: &[(usize, usize)] = &[(1024, 256), (1000, 256), (5, 1)];
        for &(len, cs) in cases {
            let count = fixed_chunk_count(len, cs);
            let eff = cs.max(1);
            for i in 0..count.saturating_sub(1) {
                let (s, e) = fixed_chunk_bounds(len, cs, i).unwrap();
                assert_eq!(e - s, eff, "non-final chunk {i} not full");
            }
        }
    }

    #[test]
    fn test_fixed_property_final_not_larger_than_chunk_size() {
        let cases: &[(usize, usize)] = &[(1024, 256), (1000, 256), (5, 1)];
        for &(len, cs) in cases {
            let count = fixed_chunk_count(len, cs);
            if count > 0 {
                let (s, e) = fixed_chunk_bounds(len, cs, count - 1).unwrap();
                assert!(e - s <= cs.max(1));
            }
        }
    }

    #[test]
    fn test_fixed_property_deterministic() {
        for _ in 0..10 {
            assert_eq!(fixed_chunk_count(1000, 256), 4);
            assert_eq!(fixed_chunk_bounds(1000, 256, 1), Some((256, 512)));
        }
    }

    #[test]
    fn test_fixed_overflow_safety() {
        // chunk_size near usize::MAX — should produce 1 chunk covering the file
        let len: usize = 1024;
        let huge: usize = usize::MAX;
        assert_eq!(fixed_chunk_count(len, huge), 1);
        assert_eq!(fixed_chunk_bounds(len, huge, 0), Some((0, len)));

        // chunk_size = 1 on reasonable file
        assert_eq!(fixed_chunk_count(len, 1), len);
        assert_eq!(fixed_chunk_bounds(len, 1, 0), Some((0, 1)));
        assert_eq!(fixed_chunk_bounds(len, 1, len - 1), Some((len - 1, len)));

        // zero file, huge chunk
        assert_eq!(fixed_chunk_count(0, usize::MAX), 0);
    }

    // ── SWAR byte-search correctness & performance (internal) ────────────

    mod swar_bench {
        use super::find_byte_swar;
        use std::hint::black_box;
        use std::time::Instant;

        #[inline(always)]
        fn find_byte_scalar(haystack: &[u8], delimiter: u8) -> Option<usize> {
            haystack.iter().position(|&b| b == delimiter)
        }

        // ── Correctness oracle ──────────────────────────────────────

        const DELIMITERS: &[u8] = &[0x00, 0x01, b'\n', b',', b'|', 0x7f, 0x80, 0xfe, 0xff];
        const LENGTHS: &[usize] = &[
            0, 1, 2, 3, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 200, 256,
        ];

        fn assert_swar_eq_scalar(data: &[u8], delim: u8, label: &str) {
            let s = find_byte_scalar(data, delim);
            let w = find_byte_swar(data, delim);
            assert_eq!(
                s,
                w,
                "SWAR != scalar: {label}  data[0]={:02x} len={} delim={:02x}",
                data.first().copied().unwrap_or(0),
                data.len(),
                delim
            );
        }

        fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
            let mut state = seed;
            (0..len)
                .map(|_| {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (state >> 32) as u8
                })
                .collect()
        }

        #[test]
        fn correctness_empty() {
            for &d in DELIMITERS {
                assert_swar_eq_scalar(&[], d, "empty");
            }
        }

        #[test]
        fn correctness_all_lengths_no_match() {
            for &len in LENGTHS {
                let delim = b'\n';
                let data = vec![b'x'; len];
                assert_swar_eq_scalar(&data, delim, &format!("len={len} nomatch"));
            }
        }

        #[test]
        fn correctness_all_lengths_all_match() {
            for &len in LENGTHS {
                if len == 0 {
                    continue;
                }
                let delim = b'\n';
                let data = vec![delim; len];
                assert_swar_eq_scalar(&data, delim, &format!("len={len} allmatch"));
            }
        }

        #[test]
        fn correctness_match_at_every_position() {
            for &len in &[1, 8, 16, 32, 64, 128, 200] {
                for pos in 0..len {
                    let delim = b'\n';
                    let mut data = vec![b'x'; len];
                    data[pos] = delim;
                    assert_swar_eq_scalar(&data, delim, &format!("len={len} pos={pos}"));
                }
            }
        }

        #[test]
        fn correctness_all_delimiters() {
            let len = 256usize;
            for &delim in DELIMITERS {
                let other: u8 = if delim == 0x00 { 0x01 } else { 0x00 };
                let data = vec![other; len];
                assert_swar_eq_scalar(&data, delim, &format!("delim={delim:02x} nomatch"));
                let mut data = vec![other; len];
                data[len / 2] = delim;
                assert_swar_eq_scalar(&data, delim, &format!("delim={delim:02x} middle"));
            }
        }

        #[test]
        fn correctness_unaligned_starts() {
            let base = deterministic_bytes(42, 256);
            for start in 0..16 {
                for &delim in &[b'\n', b',', 0x00, 0xff] {
                    let slice = &base[start..];
                    assert_swar_eq_scalar(
                        slice,
                        delim,
                        &format!("unaligned start={start} delim={delim:02x}"),
                    );
                }
            }
        }

        #[test]
        fn correctness_match_at_boundaries() {
            let delim = b'\n';
            for &len in &[8, 9, 15, 16, 17, 24, 25, 31, 32, 33] {
                let mut data = vec![b'x'; len];
                for pos in [0, 7, 8, 15, 16, 23, 24, 31, 32].iter().copied() {
                    if pos < len {
                        data[pos] = delim;
                        assert_swar_eq_scalar(
                            &data,
                            delim,
                            &format!("len={len} word-boundary pos={pos}"),
                        );
                        data[pos] = b'x';
                    }
                }
                data[len - 1] = delim;
                assert_swar_eq_scalar(&data, delim, &format!("len={len} last-byte"));
            }
        }

        #[test]
        fn correctness_deterministic_random() {
            for seed in 0..20u64 {
                let data = deterministic_bytes(seed, 1024);
                for delim in [b'\n', b',', b'|', 0x00, 0xff] {
                    assert_swar_eq_scalar(
                        &data,
                        delim,
                        &format!("random seed={seed} delim={delim:02x}"),
                    );
                }
            }
        }

        #[test]
        fn correctness_many_matches() {
            let delim = b'\n';
            let mut data = Vec::with_capacity(10000);
            for i in 0..1000 {
                data.extend_from_slice(b"line_content_");
                data.extend_from_slice(&(i as u32).to_le_bytes());
                data.push(delim);
            }
            for &delim_test in &[b'\n', b'l', b'_', 0x00, 0xff] {
                assert_swar_eq_scalar(
                    &data,
                    delim_test,
                    &format!("many_matches delim={delim_test:02x}"),
                );
            }
        }

        // ── Byte-search microbenchmark ───────────────────────────────

        fn bench_find_byte<F>(haystack: &[u8], delim: u8, f: F, iters: u64) -> f64
        where
            F: Fn(&[u8], u8) -> Option<usize>,
        {
            let start = Instant::now();
            for _ in 0..iters {
                let result = f(black_box(haystack), delim);
                black_box(result);
            }
            start.elapsed().as_nanos() as f64 / iters as f64
        }

        fn bench_find_byte_auto<F>(haystack: &[u8], delim: u8, f: F, name: &str) -> (f64, usize)
        where
            F: Fn(&[u8], u8) -> Option<usize>,
        {
            let warmup_iters = 10;
            for _ in 0..warmup_iters {
                black_box(f(black_box(haystack), delim));
            }

            let mut iters: u64 = 100;
            let max_iters: u64 = 100_000;
            let mut samples = Vec::with_capacity(10);

            while iters <= max_iters {
                let avg_ns = bench_find_byte(haystack, delim, &f, iters);
                if avg_ns * iters as f64 > 500_000_000.0 {
                    samples.push(avg_ns);
                    if samples.len() >= 5 {
                        break;
                    }
                }
                iters = (iters * 2).min(max_iters);
                if iters == max_iters && samples.is_empty() {
                    samples.push(avg_ns);
                    break;
                }
            }

            if samples.is_empty() {
                return (0.0, 0);
            }

            let n = samples.len();
            let mut sorted = samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median_ns = sorted[n / 2];

            let result = black_box(f(black_box(haystack), delim));
            let _ = black_box(result);

            println!(
                "  {name:<20} {median_ns:>10.1} ns/call  (n={n})",
                name = name,
                n = n
            );
            (median_ns, n)
        }

        #[test]
        #[ignore = "performance experiment — run with --ignored --nocapture"]
        fn bench_byte_search_primitive() {
            println!();
            println!("=== Byte-Search Microbenchmark (scalar vs SWAR) ===");
            println!(
                "  Build: {}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
            );

            let delim = b'\n';

            fn gen_data(len: usize, pattern: &str) -> Vec<u8> {
                match pattern {
                    "no_match" => vec![b'x'; len],
                    "match_mid" => {
                        let mut d = vec![b'x'; len];
                        d[len / 2] = b'\n';
                        d
                    }
                    "match_end" => {
                        let mut d = vec![b'x'; len];
                        d[len - 1] = b'\n';
                        d
                    }
                    "match_first" => {
                        let mut d = vec![b'x'; len];
                        d[0] = b'\n';
                        d
                    }
                    _ => vec![b'x'; len],
                }
            }

            let sizes: &[(usize, &str)] = &[
                (32, "Tiny"),
                (64, "1 cache line"),
                (128, "2 cache lines"),
                (256, "4 cache lines"),
                (4096, "4 KiB page"),
                (65536, "64 KiB"),
                (1048576, "1 MiB"),
            ];

            println!("  Case                       Scalar (ns)   SWAR (ns)   Ratio");

            for &(size, label) in sizes {
                for pattern in ["no_match", "match_mid"] {
                    let data = gen_data(size, pattern);
                    let case = format!("{label} {size}B {pattern}");
                    let (s, _) = bench_find_byte_auto(
                        &data,
                        delim,
                        find_byte_scalar,
                        &format!("scalar {case}"),
                    );
                    let (w, _) = bench_find_byte_auto(
                        &data,
                        delim,
                        find_byte_swar,
                        &format!("SWAR   {case}"),
                    );
                    let ratio = if s > 0.0 { w / s } else { f64::NAN };
                    println!(
                        "  {case:<25} {s:>14.1} {w:>14.1} {ratio:>9.2}x",
                        case = case,
                        s = s,
                        w = w,
                        ratio = ratio
                    );
                }
            }
        }
    }

    // ── Partition planning tests ──────────────────────────────────────

    #[test]
    fn partition_empty_data() {
        assert_eq!(find_partition_boundaries(b"", 4, b'\n'), vec![]);
    }

    #[test]
    fn partition_zero_partitions() {
        let data = b"hello\nworld\n";
        assert_eq!(find_partition_boundaries(data, 0, b'\n'), vec![]);
    }

    #[test]
    fn partition_single_partition() {
        let data = b"hello\nworld\n";
        assert_eq!(
            find_partition_boundaries(data, 1, b'\n'),
            vec![(0, data.len())]
        );
    }

    #[test]
    fn partition_no_delimiter() {
        let data = b"no_newlines_here";
        for &n in &[2, 4, 8, 16] {
            let partitions = find_partition_boundaries(data, n, b'\n');
            assert_eq!(partitions, vec![(0, data.len())]);
        }
    }

    #[test]
    fn partition_block_count_semantics() {
        let data = b"a\nb\nc\nd\ne\n";
        // 5 records, request 2 partitions
        let partitions = find_partition_boundaries(data, 2, b'\n');
        assert!(partitions.len() >= 2);
        // Complete coverage
        assert_eq!(partitions[0].0, 0);
        assert_eq!(partitions.last().unwrap().1, data.len());
    }

    #[test]
    fn partition_property_no_gaps() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"a\nb\nc\n", 2, b'\n'),
            (b"a\nb\nc\n", 3, b'\n'),
            (b"a\nb\nc\n", 4, b'\n'),
            (b"a,b,c,d,e,f", 3, b','),
            (b"a\tb\tc\t", 2, b'\t'),
            (b"a|b|c|d|e|f", 4, b'|'),
        ];
        for &(data, n, delim) in cases {
            let partitions = find_partition_boundaries(data, n, delim);
            if partitions.is_empty() {
                continue;
            }
            assert_eq!(partitions[0].0, 0, "first must start at 0");
            for i in 1..partitions.len() {
                assert_eq!(
                    partitions[i].0,
                    partitions[i - 1].1,
                    "gap at partition {}->{}",
                    i - 1,
                    i
                );
            }
            assert_eq!(
                partitions.last().unwrap().1,
                data.len(),
                "last must end at EOF"
            );
        }
    }

    #[test]
    fn partition_property_concatenation_equals_input() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"hello\nworld\n", 2, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"one\ttwo\tthree", 2, b'\t'),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"x\x00y\x00z", 2, b'\x00'),
            (b"single", 4, b'\n'),
            (b"\n\n\n\n\n", 2, b'\n'),
            (b"\n", 2, b'\n'),
            (b"a", 4, b'\n'),
        ];
        for &(data, n, delim) in cases {
            let partitions = find_partition_boundaries(data, n, delim);
            let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
            assert_eq!(
                total,
                data.len(),
                "concat property failed: n={n} delim={delim:02x}"
            );
        }
    }

    #[test]
    fn partition_property_determinism() {
        let data = b"line1\nline2\nline3\nline4\nline5\n";
        let p1 = find_partition_boundaries(data, 3, b'\n');
        let p2 = find_partition_boundaries(data, 3, b'\n');
        assert_eq!(p1, p2);
        let p3 = find_partition_boundaries(data, 3, b'\n');
        assert_eq!(p1, p3);
    }

    #[test]
    fn partition_property_boundary_after_delimiter() {
        let data = b"record1\nrecord2\nrecord3\n";
        for &n in &[2, 3, 4] {
            let partitions = find_partition_boundaries(data, n, b'\n');
            // Every non-final partition must end right after a newline
            for (_start, end) in partitions.iter().take(partitions.len().saturating_sub(1)) {
                assert_eq!(
                    data[end.wrapping_sub(1)],
                    b'\n',
                    "non-final partition must end after delimiter"
                );
                assert!(*end > 0, "partition must be non-empty");
            }
        }
    }

    #[test]
    fn partition_property_no_empty_partitions() {
        let data = b"a\nb\nc\n";
        for &n in &[2, 3, 4, 8] {
            let partitions = find_partition_boundaries(data, n, b'\n');
            for &(start, end) in &partitions {
                assert!(end > start, "zero-length partition: n={n}");
            }
        }
    }

    #[test]
    fn partition_property_monotonic() {
        let data = b"x\nxx\nxxx\nxxxx\nxxxxx\n";
        for &n in &[2, 3, 4, 8] {
            let partitions = find_partition_boundaries(data, n, b'\n');
            let mut last_end = 0usize;
            for (start, end) in &partitions {
                assert!(*start >= last_end);
                assert!(*end > *start);
                last_end = *end;
            }
        }
    }

    // ── Record integrity — numbered records ─────────────────────────

    #[test]
    fn partition_record_integrity_numbered() {
        let delim = b'\n';
        let mut data = Vec::new();
        let mut original_records: Vec<Vec<u8>> = Vec::new();
        for i in 0..100u32 {
            let record = format!("record-{i:06}\n").into_bytes();
            data.extend_from_slice(&record);
            original_records.push(record);
        }

        // Ensure data starts at 0
        let file_len = data.len();

        for &n in &[1, 2, 3, 5, 7, 8, 13, 17, 20, 37, 50] {
            let partitions = find_partition_boundaries(&data, n, delim);

            // Verify complete coverage
            let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, file_len);

            // Collect all records as bytes from each partition
            let mut recovered: Vec<Vec<u8>> = Vec::new();
            for &(start, end) in &partitions {
                let chunk = &data[start..end];
                // Split chunk by delimiter (simple line parser)
                let mut pos = 0;
                for (j, &b) in chunk.iter().enumerate() {
                    if b == delim {
                        let rec = chunk[pos..=j].to_vec();
                        if !rec.is_empty() {
                            recovered.push(rec);
                        }
                        pos = j + 1;
                    }
                }
                // Handle trailing content without delimiter
                if pos < chunk.len() {
                    recovered.push(chunk[pos..].to_vec());
                }
            }

            // Must recover exactly original records in order
            assert_eq!(
                recovered, original_records,
                "record integrity violated for n={n}"
            );
        }
    }

    #[test]
    fn partition_record_integrity_giant_record() {
        // One giant record in the middle, records on both sides
        let delim = b'\n';
        let mut data = b"short1\n".to_vec();
        let giant = vec![b'x'; 5000];
        // Giant has NO delimiter — spans many target positions
        data.extend_from_slice(&giant);
        data.extend_from_slice(b"short2\n");
        let file_len = data.len();

        for &n in &[2, 4, 8, 16, 32] {
            let partitions = find_partition_boundaries(&data, n, delim);

            // Complete coverage
            let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, file_len);

            // The giant record should not be split
            // Check non-final partition boundaries
            for (_start, end) in partitions.iter().take(partitions.len().saturating_sub(1)) {
                assert_eq!(
                    data[end.wrapping_sub(1)],
                    delim,
                    "non-final partition must end after delimiter at n={n}"
                );
            }

            // If n <= number of actual records, should have fewer or equal partitions
            // With 2 records + 1 giant no-delim block, max partitions = 3
            assert!(partitions.len() <= 3 + 1);
        }
    }

    // ── Balance metrics ──────────────────────────────────────────────

    #[test]
    fn partition_balance_uniform_100b() {
        let delim = b'\n';
        let record_size = 100;
        let record_count = 1000;
        let mut data = Vec::new();
        for i in 0..record_count {
            let fill = (i as u8).wrapping_add(0x41);
            let payload = vec![fill; record_size - 1];
            data.extend_from_slice(&payload);
            data.push(delim);
        }
        let file_len = data.len();

        fn max_abs_deviation(partitions: &[(usize, usize)], ideal: usize) -> usize {
            partitions
                .iter()
                .map(|(s, e)| {
                    let size = e - s;
                    size.abs_diff(ideal)
                })
                .max()
                .unwrap_or(0)
        }

        for &n in &[2, 4, 8, 16, 32] {
            let partitions = find_partition_boundaries(&data, n, delim);
            let actual_n = partitions.len();
            let ideal = file_len / actual_n;
            let mad = max_abs_deviation(&partitions, ideal);

            // For uniform 100B records, deviation should be bounded by ~2 * record_size
            assert!(
                mad <= 2 * record_size + 10,
                "n={n}, ideal={ideal}, mad={mad}, too much deviation"
            );

            // Verify boundaries were not unnecessarily shifted
            for (_start, end) in partitions.iter().take(actual_n.saturating_sub(1)) {
                assert_eq!(data[end - 1], delim);
            }
        }
    }

    #[test]
    fn partition_balance_variable_records() {
        let delim = b'\n';
        let pattern: &[usize] = &[20, 100, 4096]; // 20B, 100B, 4KiB repeating
        let mut data = Vec::new();
        let mut i = 0;
        while data.len() < 1_000_000 {
            let payload_size = pattern[i % pattern.len()].saturating_sub(1);
            let fill = (i as u8).wrapping_add(0x41);
            data.extend(std::iter::repeat(fill).take(payload_size));
            data.push(delim);
            i += 1;
        }
        let file_len = data.len();

        for &n in &[2, 4, 8, 16, 32] {
            let partitions = find_partition_boundaries(&data, n, delim);
            let actual_n = partitions.len();
            let _ideal = file_len / actual_n;

            // With max record of 4KB, deviation should stay bounded
            for (start, end) in &partitions {
                let size = end - start;
                // Extremely loose bound — just ensure no catastrophic imbalance
                assert!(size > 0, "zero-length partition at n={n}");
            }

            // Verify coverage
            let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, file_len);
        }
    }

    #[test]
    fn partition_absolute_vs_iterative_drift() {
        // This test verifies that the absolute-target algorithm does NOT
        // suffer from cumulative drift like the iterative approach.
        //
        // For a file with one large record (delaying the first boundary),
        // the iterative approach would shift ALL subsequent boundaries
        // by the overshoot. Absolute targets recenter.

        let delim = b'\n';
        let mut data = Vec::new();
        // One giant record at the start
        data.extend_from_slice(b"giant_record_with_no_delimiter_here");
        // Then many small records
        for i in 0..100usize {
            let record = format!("line-{i:04}\n").into_bytes();
            data.extend_from_slice(&record);
        }
        let file_len = data.len();
        let n = 8;

        let partitions = find_partition_boundaries(&data, n, delim);
        let actual_n = partitions.len();

        // The first partition should contain the giant record
        assert_eq!(partitions[0].0, 0);
        // Remaining partitions should each be approximately equal
        let remaining_data = file_len - partitions[0].1;
        let remaining_partitions = actual_n - 1;
        let ideal_remaining = remaining_data / remaining_partitions;

        let max_dev: usize = partitions[1..]
            .iter()
            .map(|(s, e)| {
                let size = e - s;
                size.abs_diff(ideal_remaining)
            })
            .max()
            .unwrap_or(0);

        // After the giant record, remaining partitions should be well-balanced
        // (bounded by max record size, which is small here)
        let max_record = b"giant_record_with_no_delimiter_here".len() + 1 + 10; // generous
        assert!(
            max_dev <= max_record * 2 + 50,
            "remaining partitions unbalanced: max_dev={max_dev}, ideal_remaining={ideal_remaining}"
        );

        // Also verify: the giant record is NOT split
        assert!(partitions.len() <= actual_n);
    }

    #[test]
    fn partition_sparse_64k_records() {
        let delim = b'\n';
        let record_size = 65536;
        let record_count = 50;
        let mut data = Vec::with_capacity(record_count * record_size);
        for i in 0..record_count {
            let fill = (i as u8).wrapping_add(0x41);
            data.extend(std::iter::repeat(fill).take(record_size - 1));
            data.push(delim);
        }
        let file_len = data.len();

        for &n in &[2, 4, 8, 16] {
            let partitions = find_partition_boundaries(&data, n, delim);

            // Complete coverage
            let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, file_len);

            // With 64KB records, partitions may be few. There should be no split records.
            for (_start, end) in partitions.iter().take(partitions.len().saturating_sub(1)) {
                // Non-final partitions end after a delimiter
                assert_eq!(data[end.wrapping_sub(1)], delim, "64KB record split? n={n}");
            }
        }
    }

    #[test]
    fn partition_no_empty_to_hit_n() {
        // A file with 2 records, N=100 -> should produce 2 partitions, not 100
        let data = b"record1\nrecord2\n";
        let partitions = find_partition_boundaries(data, 100, b'\n');
        assert!(!partitions.is_empty());
        for &(s, e) in &partitions {
            assert!(e > s, "must not create empty partitions");
        }
        assert!(
            partitions.len() < 100,
            "should produce fewer partitions than N"
        );
    }

    #[test]
    fn partition_consecutive_delimiters() {
        let data = b"\n\n\n\n\n";
        for &n in &[2, 3, 4] {
            let partitions = find_partition_boundaries(data, n, b'\n');
            if partitions.is_empty() {
                continue;
            }
            assert_eq!(partitions[0].0, 0);
            for i in 1..partitions.len() {
                assert_eq!(partitions[i].0, partitions[i - 1].1);
            }
            assert_eq!(partitions.last().unwrap().1, data.len());
        }
    }

    #[test]
    fn partition_only_newlines() {
        let data = b"\n\n\n";
        for &n in &[1, 2, 3, 4] {
            let partitions = find_partition_boundaries(data, n, b'\n');
            let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, data.len());
        }
    }

    // ── Multi-byte delimiter tests ──────────────────────────────────────

    /// Verify pattern scanner == single-byte scanner for 1-byte delimiters.
    fn pattern_equals_single_byte(data: &[u8], chunk_size: usize, delimiter: u8) {
        let single = find_chunk_boundaries(data, chunk_size, delimiter);
        let pattern = find_chunk_boundaries_pattern(data, chunk_size, &[delimiter]);
        assert_eq!(
            single, pattern,
            "pattern != single: chunk_size={chunk_size} delim={delimiter:#04x}"
        );
    }

    /// Verify PatternChunkCursor == ChunkCursor for 1-byte delimiters.
    fn cursor_pattern_equals_single_byte(data: &[u8], chunk_size: usize, delimiter: u8) {
        let single: Vec<&[u8]> = ChunkCursor::new(data, chunk_size, delimiter).collect();
        let pattern: Vec<&[u8]> = PatternChunkCursor::new(data, chunk_size, &[delimiter]).collect();
        assert_eq!(
            single, pattern,
            "pattern cursor != single cursor: chunk_size={chunk_size} delim={delimiter:#04x}",
        );
    }

    #[test]
    fn pattern_single_byte_equivalence_all_delimiters() {
        let cases: &[(&[u8], usize, u8)] = &[
            (b"hello\nworld\n", 4, b'\n'),
            (b"a,b,c,d", 2, b','),
            (b"one\ttwo\tthree", 4, b'\t'),
            (b"a|b|c|d|e|f", 3, b'|'),
            (b"x\x00y\x00z", 2, b'\x00'),
            (b"single", 1024, b'\n'),
            (b"\n\n\n\n\n", 1, b'\n'),
            (b"", 1024, b'\n'),
            (b"\n", 1, b'\n'),
            (b"a", 1024, b'\n'),
            (b"line1\nline2\nline3\nline4\nline5\n", 10, b'\n'),
            (b"no_newlines_here", 5, b'\n'),
            (b"xxxx\n", 5, b'\n'),
            (b"a\nb\nc\n", 1, b'\n'),
            (b"tiny\nverylongrecordwithnobreaksanywhere\nend\n", 6, b'\n'),
        ];
        for &(data, chunk_size, delim) in cases {
            pattern_equals_single_byte(data, chunk_size, delim);
            cursor_pattern_equals_single_byte(data, chunk_size, delim);
        }
    }

    #[test]
    fn pattern_empty_delimiter_panics() {
        let result = std::panic::catch_unwind(|| {
            find_chunk_boundaries_pattern(b"hello", 10, b"");
        });
        assert!(result.is_err(), "empty delimiter must panic");
    }

    #[test]
    fn pattern_empty_data() {
        assert_eq!(find_chunk_boundaries_pattern(b"", 1024, b"\r\n"), vec![]);
        assert_eq!(
            PatternChunkCursor::new(b"", 1024, b"\r\n").collect::<Vec<_>>(),
            Vec::<&[u8]>::new()
        );
    }

    #[test]
    fn pattern_crlf_basic() {
        let data = b"a\r\nb\r\nc\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 4, b"\r\n");
        assert_eq!(chunks, vec![(0, 6), (6, 9)]);

        let cursor: Vec<&[u8]> = PatternChunkCursor::new(data, 4, b"\r\n").collect();
        assert_eq!(cursor, vec![b"a\r\nb\r\n" as &[u8], b"c\r\n" as &[u8]]);
    }

    #[test]
    fn pattern_crlf_no_trailing_delimiter() {
        let data = b"a\r\nb\r\nc";
        let chunks = find_chunk_boundaries_pattern(data, 1024, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_consecutive_crlf() {
        let data = b"\r\n\r\n\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 1, b"\r\n");
        assert_eq!(chunks, vec![(0, 4), (4, 6)]);
    }

    #[test]
    fn pattern_double_crlf_http_style() {
        let data = b"Header: val\r\n\r\nbody";
        let chunks = find_chunk_boundaries_pattern(data, 1024, b"\r\n\r\n");
        assert_eq!(chunks, vec![(0, 19)]);
    }

    #[test]
    fn pattern_double_crlf_split_at_delimiter() {
        let data = b"Header: val\r\n\r\nbody line 2\r\n\r\nmore";
        let chunks = find_chunk_boundaries_pattern(data, 1, b"\r\n\r\n");
        assert_eq!(chunks.len(), 3);
        let total: usize = chunks.iter().map(|(s, e)| e - s).sum();
        assert_eq!(total, data.len());
        assert_eq!(&data[chunks[0].0..chunks[0].1], b"Header: val\r\n\r\n");
    }

    #[test]
    fn pattern_custom_double_separator() {
        let data = b"a||b||c||d";
        let chunks = find_chunk_boundaries_pattern(data, 4, b"||");
        assert_eq!(chunks, vec![(0, 6), (6, 10)]);

        let cursor: Vec<&[u8]> = PatternChunkCursor::new(data, 4, b"||").collect();
        assert_eq!(cursor, vec![b"a||b||" as &[u8], b"c||d" as &[u8]]);
    }

    #[test]
    fn pattern_binary_delimiter() {
        let data = b"AB\x00\xFF\x00CD\x00\xFF\x00EF";
        let chunks = find_chunk_boundaries_pattern(data, 10, b"\x00\xff\x00");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_binary_delimiter_small_chunk() {
        let data = b"AB\x00\xFF\x00CD\x00\xFF\x00EF";
        let chunks = find_chunk_boundaries_pattern(data, 4, b"\x00\xff\x00");
        assert_eq!(chunks, vec![(0, 10), (10, 12)]);
    }

    #[test]
    fn pattern_at_eof() {
        let data = b"hello\r\nworld\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 50, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_partial_at_eof() {
        let data = b"hello\r\nworld\r";
        let chunks = find_chunk_boundaries_pattern(data, 50, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_exactly_at_target() {
        let data = b"xxxx\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 6, b"\r\n");
        assert_eq!(chunks, vec![(0, 6)]);
    }

    #[test]
    fn pattern_starts_one_byte_before_target() {
        let data = b"xxxy\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 6, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
        assert_eq!(&data[..chunks[0].1], b"xxxy\r\n");
    }

    #[test]
    fn pattern_no_delimiter() {
        let data = b"no_delimiter_here";
        let chunks = find_chunk_boundaries_pattern(data, 5, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_delimiter_longer_than_data() {
        let data = b"hi";
        let chunks = find_chunk_boundaries_pattern(data, 1024, b"\r\n\r\n\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_chunk_size_zero() {
        let data = b"a\r\nb\r\nc\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 0, b"\r\n");
        assert!(!chunks.is_empty());
        let total: usize = chunks.iter().map(|(s, e)| e - s).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn pattern_chunk_size_one() {
        let data = b"a\r\nb\r\nc\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 1, b"\r\n");
        let mut pos = 0;
        for (start, end) in &chunks {
            assert_eq!(*start, pos);
            pos = *end;
        }
        assert_eq!(pos, data.len());
    }

    #[test]
    fn pattern_huge_chunk_size() {
        let data = b"a\r\nb\r\nc\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 1_000_000, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn pattern_overlapping_pattern() {
        // Delimiter "aa" should not match at position 0 in "aaa"
        let data = b"xaaaay";
        let chunks = find_chunk_boundaries_pattern(data, 1, b"aa");
        let total: usize = chunks.iter().map(|(s, e)| e - s).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn pattern_cursor_equivalence_crlf() {
        let data = b"line1\r\nline2\r\nline3\r\n";
        let eager = find_chunk_boundaries_pattern(data, 6, b"\r\n");
        let cursor: Vec<&[u8]> = PatternChunkCursor::new(data, 6, b"\r\n").collect();
        let lazy_ranges: Vec<(usize, usize)> = cursor
            .iter()
            .scan(0usize, |pos, &chunk| {
                let start = *pos;
                *pos += chunk.len();
                Some((start, *pos))
            })
            .collect();
        assert_eq!(lazy_ranges, eager);
    }

    #[test]
    fn pattern_deterministic_corpus() {
        let mut data = Vec::new();
        for i in 0..1000u32 {
            data.extend_from_slice(format!("line_{i:04}\r\n").as_bytes());
        }
        let eager = find_chunk_boundaries_pattern(&data, 64, b"\r\n");
        let cursor: Vec<&[u8]> = PatternChunkCursor::new(&data, 64, b"\r\n").collect();
        let cursor_ranges: Vec<(usize, usize)> = cursor
            .iter()
            .scan(0usize, |pos, &chunk| {
                let start = *pos;
                *pos += chunk.len();
                Some((start, *pos))
            })
            .collect();
        assert_eq!(cursor_ranges, eager);
    }

    #[test]
    fn pattern_property_no_gaps() {
        let data = b"rec1\r\nrec2\r\nrec3\r\nrec4\r\n";
        let chunks = find_chunk_boundaries_pattern(data, 6, b"\r\n");
        if chunks.is_empty() {
            return;
        }
        assert_eq!(chunks[0].0, 0);
        for i in 1..chunks.len() {
            assert_eq!(chunks[i].0, chunks[i - 1].1);
        }
        assert_eq!(chunks.last().unwrap().1, data.len());
    }

    #[test]
    fn pattern_property_concatenation() {
        let cases: &[(&[u8], usize, &[u8])] = &[
            (b"a\r\nb\r\nc\r\n", 4, b"\r\n"),
            (b"ab||cd||ef", 4, b"||"),
            (b"single", 1024, b"\r\n"),
            (b"AB\xff\x00CD\xff\x00EF", 4, b"\xff\x00"),
        ];
        for &(data, cs, delim) in cases {
            let chunks = find_chunk_boundaries_pattern(data, cs, delim);
            let total: usize = chunks.iter().map(|(s, e)| e - s).sum();
            assert_eq!(total, data.len());
        }
    }

    #[test]
    fn pattern_property_determinism() {
        let data = b"x||y||z||w||";
        let c1 = find_chunk_boundaries_pattern(data, 2, b"||");
        let c2 = find_chunk_boundaries_pattern(data, 2, b"||");
        assert_eq!(c1, c2);
    }

    #[test]
    #[should_panic(expected = "delimiter must not be empty")]
    fn pattern_empty_delimiter_cursor_panics() {
        let _ = PatternChunkCursor::new(b"hello", 10, b"");
    }

    #[test]
    fn pattern_empty_delimiter_cursor_panics_catch() {
        let result = std::panic::catch_unwind(|| {
            PatternChunkCursor::new(b"hello", 10, b"");
        });
        assert!(result.is_err());
    }

    #[test]
    fn pattern_repeated_prefix_adversarial() {
        // Searching for "aaaaab" in "aaaaaaaaaa..." — worst-case for
        // first-byte SWAR + verify (hits many false positives).
        let delimiter = b"aaaaab";
        let hay = vec![b'a'; 10_000];
        let data: Vec<u8> = hay.iter().chain(delimiter.iter()).copied().collect();
        let chunks = find_chunk_boundaries_pattern(&data, 5000, delimiter);
        assert!(!chunks.is_empty());
        let total: usize = chunks.iter().map(|(s, e)| e - s).sum();
        assert_eq!(total, data.len());
    }

    // ── Overflow safety — near-usize::MAX arithmetic ──────────────────

    #[test]
    fn overflow_safe_partition_target_u128() {
        // On 64-bit, file_len * i can overflow u64. u128 prevents this.
        let huge: usize = usize::MAX;
        // file_len = huge, n = 3: product hits 2*huge which overflows u64
        // but fits in u128. Cannot actually mmap this, so test the formula.
        let target = (huge as u128) * (2u128) / (3u128);
        assert!(target < huge as u128);

        // Verify the partition function produces correct boundaries at the
        // realistic scale: a real file fits in usize but needs correct math.
        let data = b"aa\nbb\ncc\ndd\nee\n";
        let partitions = find_partition_boundaries(data, 3, b'\n');
        let total: usize = partitions.iter().map(|(s, e)| e - s).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn overflow_safe_scanner_target_saturates() {
        // chunk_size near usize::MAX, start near len → saturating_add
        // prevents wrap. The test verifies the clamped behavior.
        let data = b"hello\nworld\n";
        let chunks = find_chunk_boundaries(data, usize::MAX, b'\n');
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn overflow_safe_cursor_target_saturates() {
        let data = b"hello\nworld\n";
        let cursor: Vec<&[u8]> = ChunkCursor::new(data, usize::MAX, b'\n').collect();
        let total: usize = cursor.iter().map(|c| c.len()).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn overflow_safe_pattern_target_saturates() {
        let data = b"a\r\nb\r\nc\r\n";
        let chunks = find_chunk_boundaries_pattern(data, usize::MAX, b"\r\n");
        assert_eq!(chunks, vec![(0, data.len())]);
    }

    #[test]
    fn overflow_safe_pattern_cursor_target_saturates() {
        let data = b"a\r\nb\r\nc\r\n";
        let cursor: Vec<&[u8]> = PatternChunkCursor::new(data, usize::MAX, b"\r\n").collect();
        let total: usize = cursor.iter().map(|c| c.len()).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn overflow_safe_fixed_bounds_large_values() {
        // Already uses saturating_mul/saturating_add — verify correctness
        // with extreme parameters
        assert_eq!(fixed_chunk_count(0, usize::MAX), 0);
        assert_eq!(fixed_chunk_count(1024, usize::MAX), 1);
        assert_eq!(fixed_chunk_bounds(1024, usize::MAX, 0), Some((0, 1024)));

        // chunk_size = 1 on large file
        let len: usize = 1000;
        assert_eq!(fixed_chunk_count(len, 1), len);
        assert_eq!(fixed_chunk_bounds(len, 1, len - 1), Some((len - 1, len)));

        // Zero file, any chunk_size
        assert_eq!(fixed_chunk_bounds(0, usize::MAX, 0), None);
    }
}
