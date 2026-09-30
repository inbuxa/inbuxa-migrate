/*
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Fetch batches bounded by bytes as well as by count. A batch of a few
//! hundred messages is small for ordinary mail and many gigabytes for a
//! mailbox of large attachments; capping the bytes too keeps what a batch
//! holds in memory about the same whatever the mail is like.

/// The default byte cap for one fetch batch.
pub const DEFAULT_BATCH_BYTES: u64 = 32 * 1024 * 1024;

/// Splits `items` into contiguous batches of at most `max_count` items and
/// at most `max_bytes` bytes, as reported by `size`. An item whose size is
/// unknown counts as 0 bytes, so without sizes this is batching by count. An
/// item larger than `max_bytes` goes in a batch of its own: every batch has
/// at least one item.
pub fn by_count_and_bytes<T>(
    items: &[T],
    size: impl Fn(&T) -> u64,
    max_count: usize,
    max_bytes: u64,
) -> Vec<&[T]> {
    let max_count = max_count.max(1);
    let max_bytes = max_bytes.max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0u64;
    for (i, item) in items.iter().enumerate() {
        let s = size(item);
        let count = i - start;
        if count > 0 && (count >= max_count || bytes.saturating_add(s) > max_bytes) {
            out.push(&items[start..i]);
            start = i;
            bytes = 0;
        }
        bytes = bytes.saturating_add(s);
    }
    if start < items.len() {
        out.push(&items[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batches(sizes: &[u64], max_count: usize, max_bytes: u64) -> Vec<Vec<u64>> {
        by_count_and_bytes(sizes, |s| *s, max_count, max_bytes)
            .into_iter()
            .map(|b| b.to_vec())
            .collect()
    }

    #[test]
    fn respects_the_byte_cap() {
        assert_eq!(
            batches(&[10, 10, 10, 10, 10], 100, 25),
            vec![vec![10, 10], vec![10, 10], vec![10]]
        );
    }

    #[test]
    fn respects_the_count_cap() {
        assert_eq!(
            batches(&[1, 1, 1, 1, 1], 2, 1000),
            vec![vec![1, 1], vec![1, 1], vec![1]]
        );
    }

    #[test]
    fn an_item_over_the_cap_goes_alone() {
        assert_eq!(
            batches(&[5, 500, 5, 5], 100, 20),
            vec![vec![5], vec![500], vec![5, 5]]
        );
        assert_eq!(batches(&[500], 100, 20), vec![vec![500]]);
    }

    #[test]
    fn unknown_sizes_batch_by_count() {
        assert_eq!(batches(&[0, 0, 0], 2, 1), vec![vec![0, 0], vec![0]]);
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(batches(&[], 10, 10).is_empty());
    }
}
