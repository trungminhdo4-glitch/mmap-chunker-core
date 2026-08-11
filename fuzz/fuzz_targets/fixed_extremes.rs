#![no_main]

use libfuzzer_sys::fuzz_target;
use mmap_chunker_core::scanner::{fixed_chunk_bounds, fixed_chunk_count};

fn read_usize(input: &[u8], offset: usize) -> usize {
    let mut word = [0u8; 8];
    let tail = input.get(offset..).unwrap_or(&[]);
    let available = tail.len().min(word.len());
    word[..available].copy_from_slice(&tail[..available]);
    u64::from_le_bytes(word) as usize
}

fn oracle_count(file_len: usize, chunk_size: usize) -> usize {
    if file_len == 0 {
        return 0;
    }
    let size = chunk_size.max(1) as u128;
    (((file_len as u128) + size - 1) / size) as usize
}

fn oracle_bounds(file_len: usize, chunk_size: usize, index: usize) -> Option<(usize, usize)> {
    let count = oracle_count(file_len, chunk_size);
    if index >= count {
        return None;
    }
    let size = chunk_size.max(1) as u128;
    let start = index as u128 * size;
    let end = (start + size).min(file_len as u128);
    Some((start as usize, end as usize))
}

fn assert_case(file_len: usize, chunk_size: usize, index: usize) {
    let actual_count = fixed_chunk_count(file_len, chunk_size);
    let expected_count = oracle_count(file_len, chunk_size);
    assert_eq!(actual_count, expected_count);

    let actual = fixed_chunk_bounds(file_len, chunk_size, index);
    let expected = oracle_bounds(file_len, chunk_size, index);
    assert_eq!(actual, expected);

    if file_len == 0 {
        assert_eq!(actual_count, 0);
        assert!(actual.is_none());
        return;
    }

    if let Some((start, end)) = actual {
        assert!(start < end);
        assert!(end <= file_len);
    }
    assert_eq!(fixed_chunk_bounds(file_len, chunk_size, actual_count), None);
    let last = fixed_chunk_bounds(file_len, chunk_size, actual_count - 1).unwrap();
    assert_eq!(last.1, file_len);
}

fuzz_target!(|input: &[u8]| {
    let file_len = read_usize(input, 0);
    let chunk_size = read_usize(input, 8);
    let index = read_usize(input, 16);
    assert_case(file_len, chunk_size, index);

    let bit = (input.first().copied().unwrap_or(0) as usize) % (usize::BITS as usize);
    let power = 1usize << bit;
    let extremes = [
        0,
        1,
        usize::MAX,
        usize::MAX - 1,
        power,
        power.saturating_sub(1),
        power.saturating_add(1),
    ];
    for &length in &extremes {
        for &size in &extremes {
            assert_case(length, size, index);
        }
    }
});
