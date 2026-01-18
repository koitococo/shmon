use bytes::Bytes;
use std::collections::VecDeque;

const DEFAULT_LIMIT: usize = 64 * 1024;

#[derive(Debug)]
pub struct Backlog {
    limit: usize,
    current: usize,
    entries: VecDeque<Bytes>,
}

impl Backlog {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_LIMIT)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            current: 0,
            entries: VecDeque::new(),
        }
    }

    pub fn push(&mut self, data: Bytes) {
        if data.is_empty() {
            return;
        }
        self.current += data.len();
        self.entries.push_back(data);
        while self.current > self.limit {
            if let Some(front) = self.entries.pop_front() {
                self.current = self.current.saturating_sub(front.len());
            } else {
                break;
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Bytes> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Backlog;
    use bytes::Bytes;

    #[test]
    fn evicts_old_entries() {
        let mut backlog = Backlog::with_limit(10);
        backlog.push(Bytes::from_static(b"hello"));
        backlog.push(Bytes::from_static(b"world!"));
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items, vec![Bytes::from_static(b"world!")]);
    }

    #[test]
    fn handles_empty_push() {
        let mut backlog = Backlog::with_limit(10);
        backlog.push(Bytes::from_static(b""));
        assert_eq!(backlog.iter().count(), 0);
    }

    #[test]
    fn exact_limit_not_evicted() {
        let mut backlog = Backlog::with_limit(5);
        backlog.push(Bytes::from_static(b"hello")); // exactly 5 bytes
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], Bytes::from_static(b"hello"));
    }

    #[test]
    fn single_large_entry_exceeds_limit() {
        let mut backlog = Backlog::with_limit(5);
        backlog.push(Bytes::from_static(b"this is longer than limit"));
        // A single entry larger than the limit gets evicted immediately
        // because current > limit triggers the eviction loop
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn iter_order_preserved() {
        let mut backlog = Backlog::with_limit(100);
        backlog.push(Bytes::from_static(b"first"));
        backlog.push(Bytes::from_static(b"second"));
        backlog.push(Bytes::from_static(b"third"));
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Bytes::from_static(b"first"));
        assert_eq!(items[1], Bytes::from_static(b"second"));
        assert_eq!(items[2], Bytes::from_static(b"third"));
    }

    #[test]
    fn multiple_evictions() {
        let mut backlog = Backlog::with_limit(15);
        backlog.push(Bytes::from_static(b"aaaaa")); // 5 bytes
        backlog.push(Bytes::from_static(b"bbbbb")); // 10 bytes total
        backlog.push(Bytes::from_static(b"ccccc")); // 15 bytes total
        backlog.push(Bytes::from_static(b"ddddd")); // 20 bytes → evicts "aaaaa"
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Bytes::from_static(b"bbbbb"));
        assert_eq!(items[1], Bytes::from_static(b"ccccc"));
        assert_eq!(items[2], Bytes::from_static(b"ddddd"));
    }
}
