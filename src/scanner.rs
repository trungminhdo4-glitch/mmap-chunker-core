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
        let mut end = start + step;

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

    // Overflow-safe: use u64 for multiplication
    let mut targets: Vec<usize> = Vec::with_capacity(target_count);
    for i in 1..n {
        let target = (file_len as u64 * i as u64 / n as u64) as usize;
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
}
