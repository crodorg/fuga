#![allow(dead_code)]

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
}
