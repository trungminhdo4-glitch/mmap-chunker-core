#![no_main]

use libfuzzer_sys::fuzz_target;
use mmap_chunker_core::scanner::{
    find_chunk_boundaries, find_chunk_boundaries_pattern, find_partition_boundaries,
};

const MAX_SOURCE_LEN: usize = 4 * 1024;
const MAX_PATTERN_LEN: usize = 32;
const MAX_PARTITIONS: usize = 64;

fn read_usize(bytes: &[u8]) -> usize {
    if bytes == b"ZEROZERO" {
        return 0;
    }
    if bytes == b"MAXXXXXX" {
        return usize::MAX;
    }
    let mut word = [0u8; 8];
    let copied = bytes.len().min(word.len());
    word[..copied].copy_from_slice(&bytes[..copied]);
    u64::from_le_bytes(word) as usize
}

fn decode_delimiter(byte: u8) -> u8 {
    match byte {
        b'N' => 0,
        b'F' => u8::MAX,
        other => other,
    }
}

fn assert_cover(data: &[u8], ranges: &[(usize, usize)]) {
    if data.is_empty() {
        assert!(ranges.is_empty());
        return;
    }

    assert_eq!(ranges.first().copied().map(|range| range.0), Some(0));
    assert_eq!(
        ranges.last().copied().map(|range| range.1),
        Some(data.len())
    );

    let mut previous_end = 0;
    for &(start, end) in ranges {
        assert_eq!(start, previous_end);
        assert!(start < end);
        assert!(end <= data.len());
        previous_end = end;
    }
}

fn scalar_single(data: &[u8], chunk_size: usize, delimiter: u8) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let step = chunk_size.max(1);

    while start < data.len() {
        let target = start.saturating_add(step);
        let end = if target >= data.len() {
            data.len()
        } else {
            let mut position = target;
            while position < data.len() && data[position] != delimiter {
                position += 1;
            }
            position.saturating_add(1).min(data.len())
        };
        ranges.push((start, end));
        start = end;
    }
    ranges
}

fn scalar_pattern(data: &[u8], chunk_size: usize, pattern: &[u8]) -> Vec<(usize, usize)> {
    assert!(!pattern.is_empty());
    let mut ranges = Vec::new();
    let mut start = 0;
    let step = chunk_size.max(1);

    while start < data.len() {
        let target = start.saturating_add(step);
        let end = if target >= data.len() {
            data.len()
        } else {
            let mut candidate = target;
            let mut boundary = data.len();
            while candidate <= data.len().saturating_sub(pattern.len()) {
                if data[candidate..candidate + pattern.len()] == pattern[..] {
                    boundary = candidate + pattern.len();
                    break;
                }
                candidate += 1;
            }
            boundary
        };
        ranges.push((start, end));
        start = end;
    }
    ranges
}

fn scalar_partition(data: &[u8], partitions: usize, delimiter: u8) -> Vec<(usize, usize)> {
    if data.is_empty() || partitions == 0 {
        return Vec::new();
    }
    if partitions == 1 {
        return vec![(0, data.len())];
    }

    let mut cuts = Vec::new();
    let mut last_cut = 0;
    for partition in 1..partitions {
        let target = ((data.len() as u128 * partition as u128) / partitions as u128) as usize;
        if target <= last_cut {
            continue;
        }
        let mut position = target;
        while position < data.len() && data[position] != delimiter {
            position += 1;
        }
        let cut = position.saturating_add(1).min(data.len());
        cuts.push(cut);
        last_cut = cut;
        if cut == data.len() {
            break;
        }
    }

    let mut ranges = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0;
    for end in cuts {
        if end > start {
            ranges.push((start, end));
        }
        start = end;
    }
    if start < data.len() {
        ranges.push((start, data.len()));
    }
    ranges
}

fuzz_target!(|input: &[u8]| {
    let Some((&mode, rest)) = input.split_first() else {
        return;
    };

    match mode % 3 {
        0 if rest.len() >= 9 => {
            let delimiter = decode_delimiter(rest[0]);
            let chunk_size = read_usize(&rest[1..9]);
            let data = &rest[9..rest.len().min(9 + MAX_SOURCE_LEN)];
            let actual = find_chunk_boundaries(data, chunk_size, delimiter);
            let expected = scalar_single(data, chunk_size, delimiter);
            assert_eq!(actual, expected);
            assert_cover(data, &actual);
            assert_eq!(find_chunk_boundaries(data, chunk_size, delimiter), actual);
        }
        1 if rest.len() >= 10 => {
            let declared_len = (rest[0] as usize % MAX_PATTERN_LEN) + 1;
            let available = rest.len() - 9;
            let pattern_len = declared_len.min(available);
            let pattern = &rest[9..9 + pattern_len];
            let data_start = 9 + pattern_len;
            let data = &rest[data_start..rest.len().min(data_start + MAX_SOURCE_LEN)];
            let chunk_size = read_usize(&rest[1..9]);
            let actual = find_chunk_boundaries_pattern(data, chunk_size, pattern);
            let expected = scalar_pattern(data, chunk_size, pattern);
            assert_eq!(actual, expected);
            assert_cover(data, &actual);
            assert_eq!(
                find_chunk_boundaries_pattern(data, chunk_size, pattern),
                actual
            );
        }
        2 if rest.len() >= 2 => {
            let delimiter = decode_delimiter(rest[0]);
            let partitions = (rest[1] as usize % MAX_PARTITIONS) + 1;
            let data = &rest[2..rest.len().min(2 + MAX_SOURCE_LEN)];
            let actual = find_partition_boundaries(data, partitions, delimiter);
            let expected = scalar_partition(data, partitions, delimiter);
            assert_eq!(actual, expected);
            assert_cover(data, &actual);
            for (index, &(_, end)) in actual.iter().enumerate() {
                if index + 1 < actual.len() {
                    assert_eq!(data[end - 1], delimiter);
                }
            }
            assert_eq!(
                find_partition_boundaries(data, partitions, delimiter),
                actual
            );
        }
        _ => {}
    }
});
