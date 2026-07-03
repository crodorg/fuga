use crate::types::ItemDisplay;

#[derive(Debug, Clone)]
pub struct QueuedItem {
    pub source_scheme: &'static str,
    pub uri: String,
    pub display: ItemDisplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    Track,
    All,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::Track,
            RepeatMode::Track => RepeatMode::Off,
        }
    }
    pub fn icon(self) -> &'static str {
        // Monochrome ASCII glyphs so the theme color carries through. Avoid
        // colorful emojis that override the foreground.
        match self {
            RepeatMode::Off => "",
            RepeatMode::All => "↻",
            RepeatMode::Track => "↻1",
        }
    }
}

#[derive(Debug, Default)]
pub struct Queue {
    items: Vec<QueuedItem>,
    current: Option<usize>,
    /// Prefix size of "manual" items — user-enqueued via `a`. The auto-queue
    /// (everything past `manual_count`) is the implicit list from whichever
    /// browse view spawned the last activation; selecting a different track
    /// replaces that suffix without disturbing the manual prefix.
    manual_count: usize,
    /// Where the next `push_manual` should insert. Tracks consecutive manual
    /// queues so each new one lands AFTER the previously queued one. Cleared
    /// whenever `current` changes (set_current / advance / back / clear) so a
    /// later queue restarts from `current+1`.
    next_manual_pos: Option<usize>,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a single item. Used by IPC `:play <uri>` and similar one-shots.
    /// Does NOT mark it as manual — manual = explicit `a` action only.
    pub fn push(&mut self, item: QueuedItem) {
        self.items.push(item);
    }

    /// Insert a user-pinned item directly after the currently-playing track
    /// (Spotify "play next" semantics). Subsequent manual queues append in
    /// order, so the second queued track lands after the first queued track,
    /// not between current and first. When nothing is playing, the item lands
    /// at the end of the manual prefix.
    pub fn push_manual(&mut self, item: QueuedItem) {
        let pos = self
            .next_manual_pos
            .unwrap_or_else(|| self.current.map(|c| c + 1).unwrap_or(self.manual_count));
        let pos = pos.min(self.items.len());
        self.items.insert(pos, item);
        // Bump `current` if the insert pushed it down.
        if let Some(c) = self.current.as_mut() {
            if *c >= pos {
                *c += 1;
            }
        }
        if pos <= self.manual_count {
            self.manual_count += 1;
        }
        self.next_manual_pos = Some(pos + 1);
    }

    /// Replace the auto-queue (everything past `manual_count`) with `items`.
    /// `current_offset_in_items` selects which one becomes current — the new
    /// `current` index will be `manual_count + current_offset_in_items`.
    pub fn replace_auto(&mut self, items: Vec<QueuedItem>, current_offset_in_items: usize) {
        self.items.truncate(self.manual_count);
        let chosen = self.manual_count + current_offset_in_items.min(items.len().saturating_sub(1));
        self.items.extend(items);
        if !self.items.is_empty() {
            self.current = Some(chosen.min(self.items.len() - 1));
        } else {
            self.current = None;
        }
        self.next_manual_pos = None;
    }

    pub fn manual_count(&self) -> usize {
        self.manual_count
    }

    pub fn items(&self) -> &[QueuedItem] {
        &self.items
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&QueuedItem> {
        self.current.and_then(|i| self.items.get(i))
    }

    pub fn set_current(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.current = Some(idx);
            self.next_manual_pos = None;
        }
    }

    pub fn advance(&mut self) -> Option<&QueuedItem> {
        let next = self.current.map_or(0, |i| i + 1);
        if next < self.items.len() {
            self.current = Some(next);
            self.next_manual_pos = None;
            self.items.get(next)
        } else {
            None
        }
    }

    /// Advance considering shuffle + repeat. `shuffle = true` picks a random
    /// other index; `repeat = Track` stays on current; `repeat = All` wraps
    /// around to 0 at the end; `repeat = Off` falls off the end (returns
    /// None).
    pub fn advance_with(&mut self, shuffle: bool, repeat: RepeatMode) -> Option<&QueuedItem> {
        if self.items.is_empty() {
            return None;
        }
        if matches!(repeat, RepeatMode::Track) {
            if let Some(i) = self.current {
                return self.items.get(i);
            }
        }
        let len = self.items.len();
        if shuffle {
            // Cheap "random" without pulling in rand: hash wall-clock nanos.
            let pseudo = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize)
                .unwrap_or(0);
            let mut next = pseudo % len;
            if Some(next) == self.current && len > 1 {
                next = (next + 1) % len;
            }
            self.current = Some(next);
            self.next_manual_pos = None;
            return self.items.get(next);
        }
        let next = self.current.map_or(0, |i| i + 1);
        if next < len {
            self.current = Some(next);
            self.next_manual_pos = None;
            return self.items.get(next);
        }
        if matches!(repeat, RepeatMode::All) {
            self.current = Some(0);
            self.next_manual_pos = None;
            return self.items.first();
        }
        None
    }

    pub fn back(&mut self) -> Option<&QueuedItem> {
        let prev = self.current.and_then(|i| i.checked_sub(1))?;
        self.current = Some(prev);
        self.next_manual_pos = None;
        self.items.get(prev)
    }

    /// Remove the item at `idx`. Adjusts `current`: shifts down if the
    /// removed item sat before current; clears if it WAS current (caller
    /// should stop playback / advance separately). Returns true if removed.
    pub fn remove(&mut self, idx: usize) -> bool {
        if idx >= self.items.len() {
            return false;
        }
        self.items.remove(idx);
        if idx < self.manual_count {
            self.manual_count -= 1;
        }
        match self.current {
            Some(c) if c == idx => self.current = None,
            Some(c) if c > idx => self.current = Some(c - 1),
            _ => {}
        }
        self.next_manual_pos = None;
        true
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.manual_count = 0;
        self.next_manual_pos = None;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> QueuedItem {
        QueuedItem {
            source_scheme: "local",
            uri: id.into(),
            display: ItemDisplay {
                title: id.into(),
                artist: None,
                album: None,
                art_uri: None,
                art_uri_full: None,
                duration: None,
                sort_hint: None,
                track_no: None,
                year_hint: None,
            },
        }
    }

    fn auto_queue(n: usize) -> Queue {
        let mut q = Queue::new();
        let items: Vec<_> = (0..n).map(|i| item(&format!("t{i}"))).collect();
        q.replace_auto(items, 0);
        q
    }

    #[test]
    fn push_manual_into_empty_lands_at_zero() {
        let mut q = Queue::new();
        q.push_manual(item("new"));
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].uri, "new");
        assert_eq!(q.manual_count, 1);
    }

    #[test]
    fn push_manual_while_playing_lands_after_current() {
        let mut q = auto_queue(10);
        q.set_current(5);
        q.push_manual(item("new"));
        // current should still point at the same track (was t5, now at idx 6
        // because new was inserted at idx 6 ... wait, inserted AT idx 6, so
        // current at 5 isn't bumped, "new" is at 6, t5..t9 shift to 6..11).
        // But current was 5, t5 is still at idx 5. new is at idx 6.
        assert_eq!(q.current, Some(5));
        assert_eq!(q.items[5].uri, "t5");
        assert_eq!(q.items[6].uri, "new");
        assert_eq!(q.items[7].uri, "t6");
    }

    #[test]
    fn push_manual_twice_appends_in_order() {
        let mut q = auto_queue(10);
        q.set_current(5);
        q.push_manual(item("a"));
        q.push_manual(item("b"));
        assert_eq!(q.items[5].uri, "t5"); // current
        assert_eq!(q.items[6].uri, "a"); // first queued
        assert_eq!(q.items[7].uri, "b"); // second queued — AFTER a, not between t5 and a
        assert_eq!(q.items[8].uri, "t6");
    }

    #[test]
    fn advance_resets_manual_cursor() {
        let mut q = auto_queue(10);
        q.set_current(5);
        q.push_manual(item("a")); // at idx 6
        let _ = q.advance(); // now playing idx 6 = "a"
        q.push_manual(item("b")); // should land at idx 7, after current ("a")
        assert_eq!(q.current, Some(6));
        assert_eq!(q.items[6].uri, "a");
        assert_eq!(q.items[7].uri, "b");
    }

    #[test]
    fn set_current_resets_manual_cursor() {
        let mut q = auto_queue(10);
        q.set_current(5);
        q.push_manual(item("a")); // idx 6
        q.set_current(2);
        q.push_manual(item("b")); // should land at idx 3 (after new current), not 7
        assert_eq!(q.current, Some(2));
        assert_eq!(q.items[3].uri, "b");
    }

    #[test]
    fn push_appends_and_grows_len() {
        let mut q = Queue::new();
        q.push(item("a"));
        q.push(item("b"));
        assert_eq!(q.len(), 2);
        assert_eq!(q.items()[1].uri, "b");
        assert_eq!(q.manual_count(), 0); // push is not manual
    }

    #[test]
    fn manual_count_accessor_tracks_manual_pushes() {
        // Into an empty queue, manual pushes form the manual prefix and DO
        // bump manual_count (unlike a push_manual landing in the auto region).
        let mut q = Queue::new();
        assert_eq!(q.manual_count(), 0);
        q.push_manual(item("m0"));
        q.push_manual(item("m1"));
        assert_eq!(q.manual_count(), 2);
    }

    #[test]
    fn replace_auto_offset_accounts_for_manual_prefix() {
        let mut q = Queue::new();
        q.push_manual(item("m0")); // manual_count = 1, at idx 0
        q.replace_auto(vec![item("a0"), item("a1"), item("a2")], 1);
        // chosen = manual_count(1) + offset(1) = idx 2 = "a1"
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(q.current().map(|i| i.uri.as_str()), Some("a1"));
    }

    #[test]
    fn advance_with_no_repeat_advances_one() {
        let mut q = auto_queue(3);
        q.set_current(0);
        let nxt = q
            .advance_with(false, RepeatMode::Off)
            .map(|i| i.uri.clone());
        assert_eq!(nxt.as_deref(), Some("t1"));
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn advance_with_repeat_off_falls_off_end() {
        let mut q = auto_queue(2);
        q.set_current(1); // last
        assert!(q.advance_with(false, RepeatMode::Off).is_none());
        assert_eq!(q.current_index(), Some(1)); // unchanged
    }

    #[test]
    fn advance_with_repeat_all_wraps_to_start() {
        let mut q = auto_queue(3);
        q.set_current(2); // last
        let nxt = q
            .advance_with(false, RepeatMode::All)
            .map(|i| i.uri.clone());
        assert_eq!(nxt.as_deref(), Some("t0"));
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn advance_with_repeat_track_stays_put() {
        let mut q = auto_queue(3);
        q.set_current(1);
        let nxt = q
            .advance_with(false, RepeatMode::Track)
            .map(|i| i.uri.clone());
        assert_eq!(nxt.as_deref(), Some("t1"));
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn advance_with_empty_returns_none() {
        let mut q = Queue::new();
        assert!(q.advance_with(false, RepeatMode::All).is_none());
        assert!(q.advance_with(true, RepeatMode::Track).is_none());
    }

    #[test]
    fn advance_with_shuffle_single_item_stays() {
        let mut q = auto_queue(1);
        q.set_current(0);
        // len == 1: the only valid pick is the single item.
        let nxt = q.advance_with(true, RepeatMode::Off).map(|i| i.uri.clone());
        assert_eq!(nxt.as_deref(), Some("t0"));
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn back_moves_to_previous() {
        let mut q = auto_queue(3);
        q.set_current(2);
        let prev = q.back().map(|i| i.uri.clone());
        assert_eq!(prev.as_deref(), Some("t1"));
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn back_at_start_returns_none() {
        let mut q = auto_queue(3);
        q.set_current(0);
        assert!(q.back().is_none());
        assert_eq!(q.current_index(), Some(0)); // unchanged
    }

    #[test]
    fn remove_before_current_shifts_current_down() {
        let mut q = auto_queue(4);
        q.set_current(2); // t2
        assert!(q.remove(0)); // drop t0
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.current().map(|i| i.uri.as_str()), Some("t2"));
    }

    #[test]
    fn remove_current_clears_current() {
        let mut q = auto_queue(4);
        q.set_current(2);
        assert!(q.remove(2));
        assert_eq!(q.current_index(), None);
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn remove_after_current_keeps_current() {
        let mut q = auto_queue(4);
        q.set_current(1);
        assert!(q.remove(3));
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.current().map(|i| i.uri.as_str()), Some("t1"));
    }

    #[test]
    fn remove_out_of_bounds_is_false() {
        let mut q = auto_queue(2);
        assert!(!q.remove(5));
        assert_eq!(q.len(), 2);
    }
}

#[cfg(test)]
mod prop {
    use super::*;
    use proptest::prelude::*;

    fn item(id: &str) -> QueuedItem {
        QueuedItem {
            source_scheme: "local",
            uri: id.into(),
            display: ItemDisplay {
                title: id.into(),
                ..Default::default()
            },
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Push,
        PushManual,
        ReplaceAuto(u8, u8),
        SetCurrent(u8),
        Advance,
        AdvanceWith(bool, u8),
        Back,
        Remove(u8),
        Clear,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::Push),
            Just(Op::PushManual),
            (0u8..8, 0u8..8).prop_map(|(n, o)| Op::ReplaceAuto(n, o)),
            (0u8..16).prop_map(Op::SetCurrent),
            Just(Op::Advance),
            (any::<bool>(), 0u8..3).prop_map(|(s, r)| Op::AdvanceWith(s, r)),
            Just(Op::Back),
            (0u8..16).prop_map(Op::Remove),
            Just(Op::Clear),
        ]
    }

    fn repeat_of(n: u8) -> RepeatMode {
        match n % 3 {
            0 => RepeatMode::Off,
            1 => RepeatMode::Track,
            _ => RepeatMode::All,
        }
    }

    /// Structural invariants that must hold after *every* operation, whatever
    /// the sequence of calls.
    fn assert_invariants(q: &Queue) {
        let len = q.len();
        assert_eq!(len, q.items().len(), "len() disagrees with items()");
        if let Some(c) = q.current_index() {
            assert!(c < len, "current index {c} out of bounds (len {len})");
        }
        assert!(
            q.manual_count() <= len,
            "manual_count {} exceeds len {len}",
            q.manual_count()
        );
        assert_eq!(
            q.current().is_some(),
            q.current_index().is_some(),
            "current() and current_index() disagree on presence"
        );
        if let Some(c) = q.current_index() {
            assert_eq!(
                q.current().map(|it| it.uri.as_str()),
                q.items().get(c).map(|it| it.uri.as_str()),
                "current() is not the item at current_index()"
            );
        }
        assert_eq!(q.is_empty(), len == 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]

        /// No sequence of queue operations may panic or leave the queue in a
        /// state that violates its structural invariants.
        #[test]
        fn queue_invariants_hold_under_random_ops(
            ops in proptest::collection::vec(op(), 0..50)
        ) {
            let mut q = Queue::new();
            let mut n = 0usize;
            assert_invariants(&q);
            for o in ops {
                match o {
                    Op::Push => { n += 1; q.push(item(&format!("p{n}"))); }
                    Op::PushManual => { n += 1; q.push_manual(item(&format!("m{n}"))); }
                    Op::ReplaceAuto(k, off) => {
                        let items: Vec<_> =
                            (0..k as usize).map(|i| item(&format!("a{i}"))).collect();
                        q.replace_auto(items, off as usize);
                    }
                    Op::SetCurrent(i) => q.set_current(i as usize),
                    Op::Advance => { let _ = q.advance(); }
                    Op::AdvanceWith(s, r) => { let _ = q.advance_with(s, repeat_of(r)); }
                    Op::Back => { let _ = q.back(); }
                    Op::Remove(i) => { let _ = q.remove(i as usize); }
                    Op::Clear => q.clear(),
                }
                assert_invariants(&q);
            }
        }

        /// clear() yields a fully empty queue regardless of prior state.
        #[test]
        fn clear_always_empties(ops in proptest::collection::vec(op(), 0..30)) {
            let mut q = Queue::new();
            let mut n = 0usize;
            for o in ops {
                match o {
                    Op::Push | Op::PushManual => { n += 1; q.push(item(&format!("x{n}"))); }
                    Op::ReplaceAuto(k, off) => {
                        let items: Vec<_> =
                            (0..k as usize).map(|i| item(&format!("a{i}"))).collect();
                        q.replace_auto(items, off as usize);
                    }
                    _ => {}
                }
            }
            q.clear();
            prop_assert_eq!(q.len(), 0);
            prop_assert!(q.is_empty());
            prop_assert_eq!(q.current_index(), None);
            prop_assert_eq!(q.manual_count(), 0);
        }
    }
}
