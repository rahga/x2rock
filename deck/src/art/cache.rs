//! Decoded-pixel budget, with recency measured by rendering rather than arrival.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const BUDGET: usize = 64 * 1024 * 1024;
pub const TRANSIENT_RETRY: Duration = Duration::from_secs(30);
pub const PERMANENT_RETRY: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Size {
    Thumbnail,
    Full,
}

impl Size {
    pub const fn max_side(self) -> u32 {
        match self {
            Self::Thumbnail => 168,
            Self::Full => 512,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub url: String,
    pub size: Size,
}

impl Key {
    pub fn new(url: &str, size: Size) -> Self {
        Self {
            url: url.to_owned(),
            size,
        }
    }
}

struct RenderPass {
    surface: usize,
    keys: HashSet<Key>,
    order: Vec<Key>,
}

#[derive(Clone, Copy)]
pub enum Surface {
    Rooms,
    Pane,
    List,
}

#[derive(Debug, PartialEq)]
pub enum State<T> {
    Loading,
    Ready(T),
    Failed { retry_at: Instant },
    // No room while existing rendered covers are protected. Keep a placeholder
    // rather than repeatedly fetching and evicting covers on an oversized list.
    Deferred { bytes: usize },
}

struct Entry<T> {
    state: State<T>,
    bytes: usize,
    rendered: u64,
}

pub struct Cache<T> {
    entries: HashMap<Key, Entry<T>>,
    budget: usize,
    used: usize,
    clock: u64,
    shown: [HashSet<Key>; 3],
    painting: Option<RenderPass>,
    row_order: [Vec<Key>; 3],
}

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self::new(BUDGET)
    }
}

impl<T> Cache<T> {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget,
            used: 0,
            clock: 0,
            shown: Default::default(),
            painting: None,
            row_order: Default::default(),
        }
    }

    pub fn begin(&mut self, surface: Surface) {
        assert!(self.painting.is_none(), "art render passes do not nest");
        self.painting = Some(RenderPass {
            surface: surface as usize,
            keys: HashSet::new(),
            order: Vec::new(),
        });
    }

    pub fn get(&mut self, url: &Key, now: Instant) -> Option<&State<T>> {
        if let Some(pass) = &mut self.painting
            && pass.keys.insert(url.clone())
        {
            pass.order.push(url.clone());
        }
        if self.entries.get(url).is_some_and(
            |entry| matches!(entry.state, State::Failed { retry_at } if now >= retry_at),
        ) {
            self.remove(url);
        }
        let entry = self.entries.get_mut(url)?;
        if matches!(entry.state, State::Ready(_)) {
            self.clock = self.clock.saturating_add(1);
            entry.rendered = self.clock;
        }
        Some(&entry.state)
    }

    pub fn peek(&self, url: &Key) -> Option<&State<T>> {
        self.entries.get(url).map(|entry| &entry.state)
    }

    fn protected(&self, url: &Key) -> bool {
        self.shown.iter().any(|urls| urls.contains(url))
            || self
                .painting
                .as_ref()
                .is_some_and(|pass| pass.keys.contains(url))
    }

    fn remove(&mut self, url: &Key) {
        if let Some(entry) = self.entries.remove(url) {
            self.used -= entry.bytes;
        }
    }

    fn insert(&mut self, url: Key, state: State<T>, bytes: usize) {
        self.remove(&url);
        self.used += bytes;
        self.entries.insert(
            url,
            Entry {
                state,
                bytes,
                rendered: 0,
            },
        );
    }

    pub fn loading(&mut self, url: Key) {
        self.insert(url, State::Loading, 0);
    }

    pub fn failed(&mut self, url: Key, retry_at: Instant) {
        self.insert(url, State::Failed { retry_at }, 0);
    }

    /// Only displayed failures need a wakeup. Hidden failures retain their
    /// deadline, so returning to a page during its cooldown does not retry.
    pub fn next_retry(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter_map(|(url, entry)| match entry.state {
                State::Failed { retry_at } if self.protected(url) => Some(retry_at),
                _ => None,
            })
            .min()
    }

    pub fn ready(&mut self, url: Key, value: T, bytes: usize) -> bool {
        self.remove(&url);
        let pinned: usize = self
            .entries
            .iter()
            .filter(|(url, _)| self.protected(url))
            .map(|(_, entry)| entry.bytes)
            .sum();
        if bytes > self.budget.saturating_sub(pinned) {
            self.insert(url, State::Deferred { bytes }, 0);
            return false;
        }
        while bytes > self.budget - self.used {
            let victim = self
                .entries
                .iter()
                .filter(|(url, entry)| entry.bytes > 0 && !self.protected(url))
                .min_by_key(|(_, entry)| entry.rendered)
                .map(|(url, _)| url.clone())
                .expect("unpinned bytes can make room for the cover");
            self.remove(&victim);
        }
        self.insert(url, State::Ready(value), bytes);
        true
    }

    /// Finish one surface, retaining the other two even on partial tick renders.
    /// The caller starts any deferred loads that can now fit; marking Loading
    /// prevents subsequent render passes from submitting the same URL again.
    pub fn end(&mut self, now: Instant) -> Vec<Key> {
        let pass = self.painting.take().expect("an art render pass is open");
        self.shown[pass.surface] = pass.keys;
        self.row_order[pass.surface] = pass.order;
        let shown = &self.shown;
        self.entries.retain(|url, entry| {
            match entry.state {
                State::Deferred { .. } => shown.iter().any(|urls| urls.contains(url)),
                // Expired hidden failures no longer need to occupy metadata.
                State::Failed { retry_at } if retry_at <= now => {
                    shown.iter().any(|urls| urls.contains(url))
                }
                _ => true,
            }
        });
        let pinned: usize = self
            .entries
            .iter()
            .filter(|(url, _)| self.protected(url))
            .map(|(_, entry)| entry.bytes)
            .sum();
        let mut available = self.budget - pinned;
        // Preserve the order covers were requested by the model, not URL order.
        let mut seen = HashSet::new();
        let deferred: Vec<_> = self
            .row_order
            .iter()
            .flatten()
            .filter_map(|key| {
                if !seen.insert(key) {
                    return None;
                }
                match self.entries.get(key)?.state {
                    State::Deferred { bytes } => Some((key.clone(), bytes)),
                    _ => None,
                }
            })
            .collect();
        deferred
            .into_iter()
            .filter_map(|(url, bytes)| {
                if bytes > available {
                    return None;
                }
                available -= bytes;
                Some(url)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(url: &str) -> Key {
        Key::new(url, Size::Full)
    }

    #[test]
    fn two_hundred_list_covers_fit_and_keep_full_art_separate() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let keys: Vec<_> = (0..200)
            .map(|i| Key::new(&format!("cover-{i}"), Size::Thumbnail))
            .collect();
        cache.begin(Surface::List);
        for key in &keys {
            cache.get(key, now);
        }
        cache.end(now);
        let side = Size::Thumbnail.max_side() as usize;
        for (i, key) in keys.iter().enumerate() {
            assert!(
                cache.ready(key.clone(), i, side * side * 4),
                "list cover {i} must fit"
            );
        }
        let full = Key::new("cover-0", Size::Full);
        assert!(cache.ready(full.clone(), 999, 512 * 512 * 4));
        assert_eq!(cache.peek(&full), Some(&State::Ready(999)));
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(cache.peek(key), Some(&State::Ready(i)));
        }
        cache.begin(Surface::List);
        for key in &keys {
            cache.get(key, now);
        }
        assert!(
            cache.end(now).is_empty(),
            "no covers are deferred for this list"
        );
        assert!(cache.used <= BUDGET);
    }

    #[test]
    fn deferred_covers_are_retried_in_row_order_not_url_order() {
        let mut cache = Cache::new(8);
        cache.ready(k("old"), "old", 8);
        paint(
            &mut cache,
            Surface::List,
            &["old", "z-first-row", "a-second-row"],
        );
        assert!(!cache.ready(k("z-first-row"), "first", 4));
        assert!(!cache.ready(k("a-second-row"), "second", 4));
        assert_eq!(
            paint(&mut cache, Surface::List, &["z-first-row", "a-second-row"]),
            [k("z-first-row"), k("a-second-row")]
        );
    }

    fn paint(cache: &mut Cache<&str>, surface: Surface, urls: &[&str]) -> Vec<Key> {
        cache.begin(surface);
        for url in urls {
            cache.get(&k(url), Instant::now());
        }
        cache.end(Instant::now())
    }

    #[test]
    fn evicts_by_bytes_and_last_render_not_insertion_order() {
        let mut cache = Cache::new(10);
        cache.ready(k("large"), "large", 6);
        cache.ready(k("small"), "small", 4);
        paint(&mut cache, Surface::Rooms, &["small", "large"]);
        paint(&mut cache, Surface::Rooms, &[]);
        assert!(cache.ready(k("new"), "new", 3));
        assert_eq!(cache.peek(&k("large")), Some(&State::Ready("large")));
        assert_eq!(cache.peek(&k("small")), None);
        assert_eq!(cache.used, 9);
        paint(&mut cache, Surface::Rooms, &["new"]);
        paint(&mut cache, Surface::Rooms, &[]);
        assert!(cache.ready(k("next"), "next", 5));
        assert_eq!(cache.peek(&k("large")), None);
        assert_eq!(cache.used, 8);
    }

    #[test]
    fn an_oversized_visible_list_keeps_its_covers_and_defers_the_rest() {
        let mut cache = Cache::new(8);
        cache.ready(k("a"), "a", 4);
        cache.ready(k("b"), "b", 4);
        paint(&mut cache, Surface::List, &["a", "b", "c"]);
        assert!(!cache.ready(k("c"), "c", 4));
        for _ in 0..5 {
            assert!(paint(&mut cache, Surface::List, &["a", "b", "c"]).is_empty());
            assert_eq!(cache.peek(&k("a")), Some(&State::Ready("a")));
            assert_eq!(cache.peek(&k("b")), Some(&State::Ready("b")));
            assert_eq!(cache.peek(&k("c")), Some(&State::Deferred { bytes: 4 }));
            assert_eq!(cache.used, 8);
        }
        // Navigation makes room; the deferred cover is requested once.
        assert_eq!(paint(&mut cache, Surface::List, &["c"]), [k("c")]);
        cache.loading(k("c"));
        assert!(paint(&mut cache, Surface::List, &["c"]).is_empty());
        assert!(cache.ready(k("c"), "c", 4));
        assert_eq!(cache.peek(&k("a")), None);
        assert_eq!(cache.used, 8);
    }

    #[test]
    fn partial_pane_renders_do_not_unpin_the_rooms_or_list() {
        let mut cache = Cache::new(8);
        cache.ready(k("room"), "room", 4);
        cache.ready(k("list"), "list", 4);
        paint(&mut cache, Surface::Rooms, &["room"]);
        paint(&mut cache, Surface::List, &["list"]);
        paint(&mut cache, Surface::Pane, &["incoming"]);
        assert!(!cache.ready(k("incoming"), "incoming", 4));
        assert!(paint(&mut cache, Surface::Pane, &["incoming"]).is_empty());
        assert_eq!(paint(&mut cache, Surface::List, &[]), [k("incoming")]);
        cache.loading(k("incoming"));
        assert!(cache.ready(k("incoming"), "incoming", 4));
        assert_eq!(cache.peek(&k("room")), Some(&State::Ready("room")));
        assert_eq!(cache.peek(&k("list")), None);
    }

    #[test]
    fn replacements_failures_and_oversized_covers_keep_accounting_correct() {
        let mut cache = Cache::new(8);
        cache.ready(k("a"), "a", 6);
        assert!(cache.ready(k("a"), "replacement", 2));
        assert_eq!(cache.used, 2);
        assert!(!cache.ready(k("too-big"), "too-big", 9));
        assert_eq!(cache.used, 2);
        assert!(paint(&mut cache, Surface::List, &["too-big"]).is_empty());
        cache.failed(k("a"), Instant::now() + TRANSIENT_RETRY);
        assert_eq!(cache.used, 0);
        paint(&mut cache, Surface::List, &[]);
        assert_eq!(cache.peek(&k("too-big")), None);
    }

    #[test]
    fn transient_failure_retries_once_at_its_deadline() {
        let t0 = Instant::now();
        let mut cache: Cache<&str> = Cache::new(8);
        cache.failed(k("cover"), t0 + TRANSIENT_RETRY);
        cache.begin(Surface::List);
        assert!(matches!(
            cache.get(&k("cover"), t0),
            Some(State::Failed { .. })
        ));
        cache.end(t0);
        assert_eq!(cache.next_retry(), Some(t0 + Duration::from_secs(30)));
        assert!(matches!(
            cache.get(&k("cover"), t0 + Duration::from_secs(29)),
            Some(State::Failed { .. })
        ));
        assert_eq!(cache.get(&k("cover"), t0 + Duration::from_secs(30)), None);
        cache.loading(k("cover"));
        assert_eq!(
            cache.get(&k("cover"), t0 + Duration::from_secs(31)),
            Some(&State::Loading)
        );
        assert_eq!(cache.next_retry(), None);
        assert!(cache.ready(k("cover"), "recovered", 4));
        assert_eq!(
            cache.get(&k("cover"), t0 + Duration::from_secs(32)),
            Some(&State::Ready("recovered"))
        );
    }

    #[test]
    fn permanent_failure_waits_longer_and_survives_a_page_round_trip() {
        let t0 = Instant::now();
        let mut cache: Cache<&str> = Cache::new(8);
        cache.failed(k("cover"), t0 + PERMANENT_RETRY);
        cache.begin(Surface::List);
        cache.get(&k("cover"), t0);
        cache.end(t0);
        cache.begin(Surface::List);
        cache.end(t0 + Duration::from_secs(1));
        assert_eq!(cache.next_retry(), None, "a hidden cover needs no timer");
        cache.begin(Surface::List);
        assert!(matches!(
            cache.get(&k("cover"), t0 + Duration::from_secs(30)),
            Some(State::Failed { .. })
        ));
        cache.end(t0 + Duration::from_secs(30));
        assert_eq!(cache.next_retry(), Some(t0 + Duration::from_secs(1800)));
        assert!(matches!(
            cache.get(&k("cover"), t0 + Duration::from_secs(1799)),
            Some(State::Failed { .. })
        ));
        assert_eq!(cache.get(&k("cover"), t0 + Duration::from_secs(1800)), None);
    }

    #[test]
    fn partial_renders_keep_the_earliest_displayed_retry_and_prune_expired_hidden_failures() {
        let t0 = Instant::now();
        let mut cache: Cache<&str> = Cache::new(8);
        cache.failed(k("room"), t0 + TRANSIENT_RETRY);
        cache.failed(k("list"), t0 + PERMANENT_RETRY);
        cache.begin(Surface::Rooms);
        cache.get(&k("room"), t0);
        cache.end(t0);
        cache.begin(Surface::List);
        cache.get(&k("list"), t0);
        cache.end(t0);
        assert_eq!(cache.next_retry(), Some(t0 + TRANSIENT_RETRY));
        cache.begin(Surface::Pane);
        cache.end(t0);
        assert_eq!(cache.next_retry(), Some(t0 + TRANSIENT_RETRY));
        cache.begin(Surface::Rooms);
        cache.end(t0 + TRANSIENT_RETRY);
        assert_eq!(cache.peek(&k("room")), None);
        assert_eq!(cache.next_retry(), Some(t0 + PERMANENT_RETRY));
    }
}
