//! The composer's memory: Up/Down through what you sent, the draft Ctrl+C cleared, and the
//! drafts Alt+S stashed. rpi-tui's editor keeps a history of its own but never reads it, so this
//! follows upstream pi's `navigate_history` (`interactive_tui/state.rs`) instead. All of it lives
//! in the TUI's memory only.
// shortcut: history and the stash are lost on detach, persist them if the owner asks.

use serde_json::Value;

/// The most entries kept.
const LIMIT: usize = 500;

#[derive(Default)]
pub struct History {
    /// Oldest first, no repeats.
    entries: Vec<String>,
    /// While walking: the entry shown, the draft it replaced, and the text put in the composer,
    /// so an edit is seen as the end of the walk.
    walk: Option<(usize, String, String)>,
    /// What Ctrl+C last cleared from a non-empty composer.
    pub cleared: Option<String>,
    /// Alt+S drafts, newest last.
    stash: Vec<String>,
}

impl History {
    /// Fills from a replayed conversation: the owner's own TUI messages, tags already off.
    pub fn fill(&mut self, messages: &[Value]) {
        for m in messages {
            if m["origin"]["channel"] == crate::attach::CHANNEL {
                let text = super::view::clean(m["text"].as_str().unwrap_or_default());
                // The attachment lines are added again on the next send.
                let kept: Vec<&str> = (text.lines())
                    .filter(|l| !l.starts_with("[attached image: "))
                    .collect();
                self.push(&kept.join("\n"));
            }
        }
    }

    /// Records a sent message as the newest entry; a repeat moves to the front.
    pub fn push(&mut self, text: &str) {
        let text = text.trim();
        self.walk = None;
        if text.is_empty() {
            return;
        }
        self.entries.retain(|e| e != text);
        self.entries.push(text.to_string());
        if self.entries.len() > LIMIT {
            self.entries.remove(0);
        }
    }

    /// Newest first, for the Ctrl+R picker.
    pub fn newest_first(&self) -> Vec<String> {
        self.entries.iter().rev().cloned().collect()
    }

    /// Up on the composer's first row: the cleared draft when the composer is empty, else the
    /// entry before the one shown. `None` leaves the key to the editor.
    pub fn up(&mut self, current: &str) -> Option<String> {
        self.ended(current);
        if self.walk.is_none() && current.is_empty() {
            if let Some(cleared) = self.cleared.take() {
                return Some(cleared);
            }
        }
        let at = match &self.walk {
            Some((0, ..)) => return None,
            Some((at, ..)) => at - 1,
            None => self.entries.len().checked_sub(1)?,
        };
        let draft = match self.walk.take() {
            Some((_, draft, _)) => draft,
            None => current.to_string(),
        };
        let text = self.entries[at].clone();
        self.walk = Some((at, draft, text.clone()));
        Some(text)
    }

    /// Down on the composer's last row while walking: the next entry, then the draft back.
    pub fn down(&mut self, current: &str) -> Option<String> {
        self.ended(current);
        let (at, draft, _) = self.walk.take()?;
        if at + 1 == self.entries.len() {
            return Some(draft);
        }
        let text = self.entries[at + 1].clone();
        self.walk = Some((at + 1, draft, text.clone()));
        Some(text)
    }

    /// Whether `current` is an entry Up or Down put there, so they keep walking even when it
    /// is a command the autocomplete would take them for.
    pub fn walking(&self, current: &str) -> bool {
        self.walk.as_ref().is_some_and(|w| w.2 == current)
    }

    /// An edit to a recalled entry makes it the draft: the walk is over.
    fn ended(&mut self, current: &str) {
        if self.walk.as_ref().is_some_and(|w| w.2 != current) {
            self.walk = None;
        }
    }

    /// Alt+S: a draft is put aside and the composer cleared (`None` back), or with an empty
    /// composer the newest one put aside comes back.
    pub fn stash(&mut self, current: &str) -> Option<String> {
        if current.trim().is_empty() {
            return self.stash.pop();
        }
        self.stash.push(current.to_string());
        Some(String::new())
    }

    pub fn stashed(&self) -> usize {
        self.stash.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn of(entries: &[&str]) -> History {
        let mut h = History::default();
        for e in entries {
            h.push(e);
        }
        h
    }

    #[test]
    fn up_and_down_walk_the_entries_and_bring_the_draft_back() {
        let mut h = of(&["one", "two", "three"]);
        assert_eq!(h.up("draft").as_deref(), Some("three"));
        assert_eq!(h.up("three").as_deref(), Some("two"));
        assert_eq!(h.up("two").as_deref(), Some("one"));
        // The oldest stays; the key goes to the editor.
        assert_eq!(h.up("one"), None);
        assert_eq!(h.down("one").as_deref(), Some("two"));
        assert_eq!(h.down("two").as_deref(), Some("three"));
        assert_eq!(h.down("three").as_deref(), Some("draft"));
        // Past the newest, Down is the editor's again.
        assert_eq!(h.down("draft"), None);
        assert_eq!(History::default().up(""), None);
    }

    #[test]
    fn an_edit_ends_the_walk_and_becomes_the_draft() {
        let mut h = of(&["one", "two"]);
        h.up("");
        assert_eq!(h.down("two!"), None);
        assert_eq!(h.up("two!").as_deref(), Some("two"));
        assert_eq!(h.down("two").as_deref(), Some("two!"));
    }

    #[test]
    fn repeats_move_to_the_front_and_the_list_is_capped() {
        let h = of(&["a", "b", " a ", ""]);
        assert_eq!(h.newest_first(), ["a", "b"]);
        let many: Vec<String> = (0..LIMIT + 5).map(|i| i.to_string()).collect();
        let h = of(&many.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(h.newest_first().len(), LIMIT);
        assert_eq!(h.newest_first().last().unwrap(), "5");
    }

    #[test]
    fn up_on_an_empty_composer_brings_back_what_ctrl_c_cleared_first() {
        let mut h = of(&["sent"]);
        h.cleared = Some("half written".into());
        // Not while there is text: that is a walk.
        assert_eq!(h.up("x").as_deref(), Some("sent"));
        h.down("sent");
        assert_eq!(h.up("").as_deref(), Some("half written"));
        assert_eq!(h.up("half written").as_deref(), Some("sent"));
        assert_eq!(h.down("sent").as_deref(), Some("half written"));
    }

    #[test]
    fn only_the_owners_tui_messages_fill_it_untagged() {
        let mut h = History::default();
        h.fill(&[
            json!({"role": "user", "origin": {"channel": "tui", "id": 1}, "text": "first\x1b[31m"}),
            json!({"role": "user", "origin": {"channel": "discord", "id": 2}, "text": "not mine"}),
            json!({"role": "assistant", "content": []}),
            json!({"role": "user", "origin": {"channel": "tui", "id": 3},
                   "text": "look @/p/a.png\n[attached image: /p/a.png (a.png, image/png, 1 KB); open it with read]"}),
        ]);
        assert_eq!(h.newest_first(), ["look @/p/a.png", "first"]);
    }

    #[test]
    fn alt_s_stashes_and_pops_newest_first() {
        let mut h = History::default();
        assert_eq!(h.stash(""), None);
        assert_eq!(h.stash("one").as_deref(), Some(""));
        assert_eq!(h.stash("two").as_deref(), Some(""));
        assert_eq!(h.stashed(), 2);
        assert_eq!(h.stash("").as_deref(), Some("two"));
        assert_eq!(h.stash("  ").as_deref(), Some("one"));
        assert_eq!(h.stash(""), None);
    }
}
