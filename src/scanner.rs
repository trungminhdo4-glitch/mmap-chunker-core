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
fn find_byte_swar(haystack: &[u8], delimiter: u8) -> Option<usize> {
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
}
