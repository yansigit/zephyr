//! Durable inbox and outbox (DESIGN §4.2). SQLite in WAL mode with `synchronous=FULL`.

use crate::channel::Inbound;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA_VERSION: i64 = 2;

/// An inbox row the router still has to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub id: i64,
    pub channel: String,
    pub sender: String,
    pub text: String,
    pub tries: i64,
}

/// An outbox row waiting for delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    pub channel: String,
    pub recipient: String,
    pub client_id: String,
    pub text: String,
    pub created_at: i64,
}

/// The platform dedupe key for a reply: at most 20 ASCII chars, so Discord's 25-char
/// nonce still fits the adapter's `-<chunk index>` suffix.
pub fn client_id(inbox_id: i64) -> String {
    let (mut n, mut digits) = (inbox_id as u64, Vec::new());
    loop {
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.reverse();
    format!("z{}", String::from_utf8(digits).unwrap())
}

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            db.execute_batch(
                "CREATE TABLE inbox(
                    id INTEGER PRIMARY KEY,
                    channel TEXT NOT NULL,
                    platform_msg_id TEXT NOT NULL,
                    sender TEXT NOT NULL,
                    text TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'new', -- new|dispatched|done|dropped|failed
                    tries INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                    UNIQUE(channel, platform_msg_id));
                 CREATE TABLE outbox(
                    id INTEGER PRIMARY KEY,
                    inbox_id INTEGER UNIQUE REFERENCES inbox(id),
                    channel TEXT NOT NULL,
                    recipient TEXT NOT NULL,
                    client_id TEXT NOT NULL UNIQUE,
                    text TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending', -- pending|sent|failed
                    created_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 PRAGMA user_version = 1;",
            )?;
        }
        if version < 2 {
            db.execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE spend(day TEXT PRIMARY KEY, tokens INTEGER NOT NULL, cost_usd REAL NOT NULL);
                 PRAGMA user_version = 2;",
            )?;
        } else if version != SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "state.db schema version {version}, this build knows {SCHEMA_VERSION}"
            )));
        }
        Ok(Self { db })
    }

    /// Commits the message. Returns `None` when `(channel, platform_msg_id)` was already seen.
    pub fn insert_inbox(&self, m: &Inbound) -> rusqlite::Result<Option<i64>> {
        let n = self.db.execute(
            "INSERT OR IGNORE INTO inbox(channel, platform_msg_id, sender, text) VALUES (?1, ?2, ?3, ?4)",
            params![m.channel, m.platform_msg_id, m.sender, m.text],
        )?;
        Ok((n == 1).then(|| self.db.last_insert_rowid()))
    }

    pub fn set_status(&self, id: i64, status: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE inbox SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }

    /// Marks the row dispatched and counts the attempt, before the child sees it.
    pub fn mark_dispatched(&self, id: i64) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE inbox SET status = 'dispatched', tries = tries + 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn inbox_with_status(&self, status: &str) -> rusqlite::Result<Vec<InboxRow>> {
        let mut stmt = self.db.prepare(
            "SELECT id, channel, sender, text, tries FROM inbox WHERE status = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![status], |r| {
            Ok(InboxRow {
                id: r.get(0)?,
                channel: r.get(1)?,
                sender: r.get(2)?,
                text: r.get(3)?,
                tries: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Writes the reply and closes the inbox row in one transaction.
    /// A row that already has a reply keeps it, so a retried reply is never queued twice.
    pub fn reply(&mut self, row: &InboxRow, text: &str, status: &str) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO outbox(inbox_id, channel, recipient, client_id, text)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.id, row.channel, row.sender, client_id(row.id), text],
        )?;
        tx.execute(
            "UPDATE inbox SET status = ?2 WHERE id = ?1",
            params![row.id, status],
        )?;
        tx.commit()
    }

    pub fn pending_outbox(&self) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.db.prepare(
            "SELECT id, channel, recipient, client_id, text, created_at FROM outbox
             WHERE status = 'pending' ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(OutboxRow {
                id: r.get(0)?,
                channel: r.get(1)?,
                recipient: r.get(2)?,
                client_id: r.get(3)?,
                text: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_outbox_status(&self, id: i64, status: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE outbox SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }

    pub fn outbox_status(&self, client_id: &str) -> rusqlite::Result<Option<String>> {
        self.db
            .query_row(
                "SELECT status FROM outbox WHERE client_id = ?1",
                params![client_id],
                |r| r.get(0),
            )
            .optional()
    }

    /// `user` after `/pause`, `budget:<day>` after a budget trip. Either way the pause ends with `/resume`.
    /// A budget pause ends at the next UTC day (SQLite's `date('now')` is UTC).
    pub fn pause_reason(&self) -> rusqlite::Result<Option<String>> {
        self.db
            .query_row(
                "SELECT value FROM meta WHERE key = 'paused'
                 AND (value = 'user' OR value = 'budget:' || date('now'))",
                [],
                |r| r.get(0),
            )
            .optional()
    }

    /// `None` clears the pause.
    pub fn set_paused(&self, reason: Option<&str>) -> rusqlite::Result<()> {
        match reason {
            Some(r) => self.db.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('paused', ?1)",
                params![r],
            ),
            None => self.db.execute("DELETE FROM meta WHERE key = 'paused'", []),
        }?;
        Ok(())
    }

    /// The UTC day, as `YYYY-MM-DD`.
    pub fn today(&self) -> rusqlite::Result<String> {
        self.db.query_row("SELECT date('now')", [], |r| r.get(0))
    }

    /// rpi's session totals at the last settle, so each settle adds only its own usage.
    pub fn last_stats(&self) -> rusqlite::Result<(i64, f64)> {
        let v: Option<String> = self
            .db
            .query_row("SELECT value FROM meta WHERE key = 'stats'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v.and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default())
    }

    pub fn set_last_stats(&self, (tokens, cost): (i64, f64)) -> rusqlite::Result<()> {
        let v = serde_json::json!([tokens, cost]).to_string();
        self.db.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('stats', ?1)",
            params![v],
        )?;
        Ok(())
    }

    /// Adds usage to the day's total and returns the new total.
    pub fn add_spend(&self, day: &str, tokens: i64, cost: f64) -> rusqlite::Result<(i64, f64)> {
        self.db.execute(
            "INSERT INTO spend(day, tokens, cost_usd) VALUES (?1, ?2, ?3)
             ON CONFLICT(day) DO UPDATE SET tokens = tokens + excluded.tokens,
                                            cost_usd = cost_usd + excluded.cost_usd",
            params![day, tokens, cost],
        )?;
        self.spend(day)
    }

    pub fn spend(&self, day: &str) -> rusqlite::Result<(i64, f64)> {
        let t = self
            .db
            .query_row(
                "SELECT tokens, cost_usd FROM spend WHERE day = ?1",
                params![day],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(t.unwrap_or_default())
    }

    /// Writes a notice to the channel and recipient of the newest owner message, if any.
    pub fn notify_last_owner(&self, text: &str) -> rusqlite::Result<()> {
        let last: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT channel, sender FROM inbox WHERE status IN ('done', 'dispatched')
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((channel, recipient)) = last else {
            return Ok(());
        };
        // No inbox row to key on, so the client id is `b<outbox id>`. Replies are `z<base36>`.
        self.db.execute(
            "INSERT INTO outbox(channel, recipient, client_id, text)
             VALUES (?1, ?2, 'b' || (SELECT coalesce(max(id), 0) + 1 FROM outbox), ?3)",
            params![channel, recipient, text],
        )?;
        Ok(())
    }

    /// `(table, status, count)` for `zephyr status`.
    pub fn counts(&self) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let mut stmt = self.db.prepare(
            "SELECT 'inbox', status, count(*) FROM inbox GROUP BY status
             UNION ALL SELECT 'outbox', status, count(*) FROM outbox GROUP BY status",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> Inbound {
        Inbound {
            channel: "chat".into(),
            platform_msg_id: id.into(),
            sender: "1000".into(),
            text: "hi".into(),
        }
    }

    #[test]
    fn duplicate_platform_ids_are_ignored() {
        let dir = crate::test_dir("store-dedupe");
        let store = Store::open(&dir.join("state.db")).unwrap();
        assert!(store.insert_inbox(&msg("a")).unwrap().is_some());
        assert_eq!(store.insert_inbox(&msg("a")).unwrap(), None);
        let mut other = msg("a");
        other.channel = "discord".into();
        assert!(store.insert_inbox(&other).unwrap().is_some());
        assert_eq!(store.inbox_with_status("new").unwrap().len(), 2);
    }

    #[test]
    fn a_reply_is_queued_once_per_inbox_row() {
        let dir = crate::test_dir("store-reply");
        let mut store = Store::open(&dir.join("state.db")).unwrap();
        store.insert_inbox(&msg("a")).unwrap();
        let row = store.inbox_with_status("new").unwrap().remove(0);
        store.reply(&row, "one", "done").unwrap();
        store.reply(&row, "two", "done").unwrap();
        let out = store.pending_outbox().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "one");
        assert_eq!(out[0].client_id, client_id(row.id));
    }

    #[test]
    fn client_ids_stay_short() {
        assert_eq!(client_id(0), "z0");
        assert_eq!(client_id(36), "z10");
        assert!(client_id(i64::MAX).len() <= 20);
    }

    #[test]
    fn pause_survives_reopen_and_budget_pause_expires_by_day() {
        let dir = crate::test_dir("store-pause");
        let path = dir.join("state.db");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.pause_reason().unwrap(), None);
        store.set_paused(Some("user")).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.pause_reason().unwrap().as_deref(), Some("user"));
        store.set_paused(Some("budget:2000-01-01")).unwrap();
        assert_eq!(
            store.pause_reason().unwrap(),
            None,
            "a past day's budget pause has ended"
        );
        store.set_paused(None).unwrap();
        assert_eq!(store.pause_reason().unwrap(), None);
    }

    #[test]
    fn spend_and_stats_accumulate() {
        let dir = crate::test_dir("store-spend");
        let store = Store::open(&dir.join("state.db")).unwrap();
        let day = store.today().unwrap();
        assert_eq!(store.last_stats().unwrap(), (0, 0.0));
        store.set_last_stats((40, 0.5)).unwrap();
        assert_eq!(store.last_stats().unwrap(), (40, 0.5));
        assert_eq!(store.add_spend(&day, 10, 0.25).unwrap(), (10, 0.25));
        assert_eq!(store.add_spend(&day, 5, 0.25).unwrap(), (15, 0.5));
        assert_eq!(store.spend("2000-01-01").unwrap(), (0, 0.0));
    }

    #[test]
    fn notices_go_to_the_newest_owner_message() {
        let dir = crate::test_dir("store-notify");
        let mut store = Store::open(&dir.join("state.db")).unwrap();
        store.notify_last_owner("nobody yet").unwrap();
        assert!(store.pending_outbox().unwrap().is_empty());
        store.insert_inbox(&msg("a")).unwrap();
        let row = store.inbox_with_status("new").unwrap().remove(0);
        store.reply(&row, "answer", "done").unwrap();
        store.notify_last_owner("first notice").unwrap();
        store.notify_last_owner("second notice").unwrap();
        let out = store.pending_outbox().unwrap();
        let texts: Vec<_> = out
            .iter()
            .map(|o| (o.channel.as_str(), o.recipient.as_str(), o.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                ("chat", "1000", "answer"),
                ("chat", "1000", "first notice"),
                ("chat", "1000", "second notice")
            ]
        );
        assert_ne!(out[1].client_id, out[2].client_id);
    }

    #[test]
    fn a_v1_database_gains_the_new_tables() {
        let dir = crate::test_dir("store-v1");
        let path = dir.join("state.db");
        Store::open(&path).unwrap().insert_inbox(&msg("a")).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE meta; DROP TABLE spend; PRAGMA user_version = 1;")
            .unwrap();
        drop(db);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.inbox_with_status("new").unwrap().len(), 1);
        store.set_paused(Some("user")).unwrap();
    }

    #[test]
    fn reopening_keeps_rows_and_pragmas() {
        let dir = crate::test_dir("store-reopen");
        Store::open(&dir.join("state.db"))
            .unwrap()
            .insert_inbox(&msg("a"))
            .unwrap();
        let store = Store::open(&dir.join("state.db")).unwrap();
        let mode: String = store
            .db
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        let sync: i64 = store
            .db
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        assert_eq!((mode.as_str(), sync), ("wal", 2));
        assert_eq!(store.inbox_with_status("new").unwrap().len(), 1);
    }
}
