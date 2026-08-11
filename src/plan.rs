//! Internal chunk-plan state shared by the Rust and C-ABI surfaces.
//!
//! This module owns planning metadata only. File bytes and C-ABI concerns
//! remain in their respective adapters.

use crate::scanner;

#[derive(Debug)]
pub(crate) enum ChunkPlan {
    Empty,
    Ranges(Vec<(usize, usize)>),
    Fixed {
        chunk_size: usize,
        chunk_count: usize,
    },
}

impl ChunkPlan {
    #[inline]
    pub(crate) fn empty() -> Self {
        Self::Empty
    }

    #[inline]
    pub(crate) fn from_ranges(ranges: Vec<(usize, usize)>) -> Self {
        if ranges.is_empty() {
            Self::Empty
        } else {
            Self::Ranges(ranges)
        }
    }

    #[inline]
    pub(crate) fn fixed(file_len: usize, chunk_size: usize) -> Self {
        if file_len == 0 {
            return Self::Empty;
        }

        let chunk_size = chunk_size.max(1);
        let chunk_count = scanner::fixed_chunk_count(file_len, chunk_size);
        Self::Fixed {
            chunk_size,
            chunk_count,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Ranges(ranges) => ranges.len(),
            Self::Fixed { chunk_count, .. } => *chunk_count,
        }
    }

    #[inline]
    pub(crate) fn range_at(&self, index: usize, source_len: usize) -> Option<(usize, usize)> {
        match self {
            Self::Empty => None,
            Self::Ranges(ranges) => ranges.get(index).copied(),
            Self::Fixed {
                chunk_size,
                chunk_count,
            } => {
                if index >= *chunk_count {
                    return None;
                }
                scanner::fixed_chunk_bounds(source_len, *chunk_size, index)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkPlan;

    #[test]
    fn empty_plan_has_no_chunks_or_ranges() {
        let plan = ChunkPlan::empty();

        assert_eq!(plan.len(), 0);
        assert_eq!(plan.range_at(0, 100), None);
    }

    #[test]
    fn empty_range_collection_normalizes_to_empty_plan() {
        let plan = ChunkPlan::from_ranges(Vec::new());

        assert_eq!(plan.len(), 0);
        assert_eq!(plan.range_at(0, 0), None);
    }

    #[test]
    fn range_plan_returns_ranges_by_index() {
        let plan = ChunkPlan::from_ranges(vec![(0, 3), (3, 8)]);

        assert_eq!(plan.len(), 2);
        assert_eq!(plan.range_at(0, 8), Some((0, 3)));
        assert_eq!(plan.range_at(1, 8), Some((3, 8)));
        assert_eq!(plan.range_at(2, 8), None);
    }

    #[test]
    fn fixed_plan_preserves_o1_state_and_clamps_zero_size() {
        let plan = ChunkPlan::fixed(5, 0);

        assert_eq!(plan.len(), 5);
        assert_eq!(plan.range_at(0, 5), Some((0, 1)));
        assert_eq!(plan.range_at(4, 5), Some((4, 5)));
        assert_eq!(plan.range_at(5, 5), None);
    }

    #[test]
    fn fixed_plan_handles_exact_and_short_final_chunks() {
        let exact = ChunkPlan::fixed(8, 4);
        assert_eq!(exact.len(), 2);
        assert_eq!(exact.range_at(0, 8), Some((0, 4)));
        assert_eq!(exact.range_at(1, 8), Some((4, 8)));

        let remainder = ChunkPlan::fixed(9, 4);
        assert_eq!(remainder.len(), 3);
        assert_eq!(remainder.range_at(2, 9), Some((8, 9)));
    }

    #[test]
    fn fixed_plan_handles_empty_and_huge_sources_without_materializing_ranges() {
        assert_eq!(ChunkPlan::fixed(0, 1).len(), 0);

        let huge = ChunkPlan::fixed(usize::MAX, usize::MAX);
        assert_eq!(huge.len(), 1);
        assert_eq!(huge.range_at(0, usize::MAX), Some((0, usize::MAX)));
        assert_eq!(huge.range_at(1, usize::MAX), None);
    }
}
