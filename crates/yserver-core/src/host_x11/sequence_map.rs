use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub(crate) struct SequenceMap<T> {
    max_window: u64,
    entries: VecDeque<(u64, T)>,
}

impl<T> SequenceMap<T> {
    pub(crate) fn new(max_window: u64) -> Self {
        Self {
            max_window,
            entries: VecDeque::new(),
        }
    }

    pub(crate) fn insert(&mut self, seq_full: u64, value: T) {
        self.prune(seq_full);
        self.entries.push_back((seq_full, value));
    }

    pub(crate) fn take(&mut self, seq_full: u64) -> Option<T> {
        let pos = self
            .entries
            .iter()
            .position(|(entry_seq, _)| *entry_seq == seq_full)?;
        self.entries.remove(pos).map(|(_, value)| value)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn prune(&mut self, newest_seq: u64) {
        let cutoff = newest_seq.saturating_sub(self.max_window);
        while self
            .entries
            .front()
            .is_some_and(|(entry_seq, _)| *entry_seq < cutoff)
        {
            self.entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SequenceMap;

    #[test]
    fn take_returns_matching_entry() {
        let mut map = SequenceMap::new(8);
        map.insert(10, "a");
        map.insert(11, "b");
        assert_eq!(map.take(10), Some("a"));
        assert_eq!(map.take(10), None);
        assert_eq!(map.take(11), Some("b"));
    }

    #[test]
    fn insert_prunes_entries_older_than_window() {
        let mut map = SequenceMap::new(4);
        map.insert(10, "a");
        map.insert(12, "b");
        map.insert(15, "c");
        assert_eq!(map.len(), 2);
        assert_eq!(map.take(10), None);
        assert_eq!(map.take(12), Some("b"));
        assert_eq!(map.take(15), Some("c"));
    }
}
