//! The shell's own visited-URL list: most recent first, de-duplicated and capped.
//! It drives `:open` autocomplete and the `:history` page, and is saved in the
//! session. Each URL carries its visit time (Unix-epoch seconds) so `:clear history
//! <period>` can drop just a time window; `0` means "time unknown" (entries from
//! sessions written before times were recorded), which only an all-time clear removes.

/// Max visited URLs kept (also the cap persisted in the session).
const HISTORY_CAP: usize = 300;

struct Visit {
    url: String,
    at: u64,
}

#[derive(Default)]
pub(crate) struct Visited {
    entries: Vec<Visit>,
}

impl Visited {
    /// Rebuild from a session's parallel lists. Missing times (older sessions, or
    /// any length drift) are stamped `0`.
    pub(crate) fn from_saved(urls: Vec<String>, times: Vec<u64>) -> Self {
        let mut times = times.into_iter();
        let entries = urls
            .into_iter()
            .take(HISTORY_CAP)
            .map(|url| Visit {
                url,
                at: times.next().unwrap_or(0),
            })
            .collect();
        Self { entries }
    }

    /// The session's parallel lists: URLs and their visit times, same order.
    pub(crate) fn to_saved(&self) -> (Vec<String>, Vec<u64>) {
        self.entries.iter().map(|v| (v.url.clone(), v.at)).unzip()
    }

    /// Move `url` to the front, stamped `at`, dropping any older copy and anything
    /// past the cap. Internal `browser://` pages and empty URLs aren't recorded.
    pub(crate) fn record(&mut self, url: &str, at: u64) {
        if url.is_empty() || url.starts_with("browser://") {
            return;
        }
        self.entries.retain(|v| v.url != url);
        self.entries.insert(
            0,
            Visit {
                url: url.to_string(),
                at,
            },
        );
        self.entries.truncate(HISTORY_CAP);
    }

    /// Drop everything; returns how many entries were removed.
    pub(crate) fn clear(&mut self) -> usize {
        std::mem::take(&mut self.entries).len()
    }

    /// Drop the entries visited at or after `cutoff`; returns how many. Entries with
    /// an unknown time (`0`) survive.
    pub(crate) fn clear_since(&mut self, cutoff: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|v| v.at < cutoff);
        before - self.entries.len()
    }

    /// Remove the entries at positions `start..=end` (most recent first), clamped to
    /// the list; returns how many were removed.
    pub(crate) fn remove_range(&mut self, start: usize, end: usize) -> usize {
        let end = end.min(self.entries.len().saturating_sub(1));
        if self.entries.is_empty() || start > end {
            return 0;
        }
        self.entries.drain(start..=end).count()
    }

    pub(crate) fn urls(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|v| v.url.as_str())
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(v: &Visited) -> Vec<&str> {
        v.urls().collect()
    }

    #[test]
    fn record_moves_a_repeat_to_the_front_with_its_new_time() {
        let mut v = Visited::default();
        v.record("https://a.test/", 1);
        v.record("https://b.test/", 2);
        v.record("https://a.test/", 3);
        v.record("browser://history", 4);
        v.record("", 5);
        assert_eq!(urls(&v), ["https://a.test/", "https://b.test/"]);
        assert_eq!(v.to_saved().1, [3, 2]);
    }

    #[test]
    fn record_keeps_at_most_the_cap() {
        let mut v = Visited::default();
        for i in 0..HISTORY_CAP + 5 {
            v.record(&format!("https://{i}.test/"), i as u64);
        }
        assert_eq!(v.len(), HISTORY_CAP);
        assert_eq!(
            v.urls().next(),
            Some(format!("https://{}.test/", HISTORY_CAP + 4).as_str())
        );
    }

    #[test]
    fn saved_lists_round_trip_and_missing_times_become_unknown() {
        let v = Visited::from_saved(
            vec!["https://a.test/".into(), "https://b.test/".into()],
            vec![7],
        );
        assert_eq!(
            v.to_saved(),
            (
                vec!["https://a.test/".into(), "https://b.test/".into()],
                vec![7, 0]
            )
        );
    }

    #[test]
    fn clear_since_keeps_older_and_unknown_visits() {
        let mut v = Visited::from_saved(
            vec![
                "https://new.test/".into(),
                "https://old.test/".into(),
                "https://unknown.test/".into(),
            ],
            vec![200, 50, 0],
        );
        assert_eq!(v.clear_since(100), 1);
        assert_eq!(urls(&v), ["https://old.test/", "https://unknown.test/"]);
        assert_eq!(v.clear(), 2);
        assert!(v.is_empty());
    }

    #[test]
    fn remove_range_clamps_to_the_list() {
        let mut v = Visited::from_saved(
            vec![
                "https://a.test/".into(),
                "https://b.test/".into(),
                "https://c.test/".into(),
            ],
            vec![3, 2, 1],
        );
        assert_eq!(v.remove_range(1, 10), 2);
        assert_eq!(urls(&v), ["https://a.test/"]);
        assert_eq!(v.remove_range(3, 4), 0);
        assert_eq!(Visited::default().remove_range(0, 0), 0);
    }
}
