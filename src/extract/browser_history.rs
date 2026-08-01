//! Browser history reading: Chromium-family (`History` SQLite file, used
//! by Chrome/Edge/Brave/Vivaldi) and Firefox (`places.sqlite`). Each
//! visited URL becomes one lightweight indexable record — this is meant
//! to feed the same unified/hybrid search entities as local files and web
//! pages (see [`crate::entity`]), not to be a separate standalone
//! feature.
//!
//! **Read this before indexing someone else's history file.** Browser
//! history is unusually sensitive: it reveals health conditions,
//! relationships, job searches, and more. This module only *reads* the
//! file the caller points it at; it is the caller's job (the indexing
//! CLI command, gated behind an explicit opt-in folder/path, same as any
//! other indexed folder) to decide whether indexing it at all is
//! appropriate, and [`crate::entity`]'s permission model is what should
//! gate who can subsequently search it.
//!
//! Both browsers keep their history file locked while running; a copy of
//! the file (not a live path) may be needed if the browser is open —
//! callers are responsible for that copy, this module just opens
//! whatever path it's given, read-only.

use rusqlite::{Connection, OpenFlags};

/// One browser history entry, independent of which browser it came from.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    pub visit_count: i64,
    /// Last-visit time, seconds since UNIX epoch.
    pub last_visit_unix: i64,
}

impl HistoryEntry {
    pub fn indexable_text(&self) -> String {
        format!("{}\n{}", self.title, self.url)
    }
}

fn open_readonly(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// Reads a Chromium-family `History` file. Chromium's `visits.visit_time`
/// is microseconds since the Windows FILETIME epoch (1601-01-01), not
/// UNIX epoch — the conversion constant below (`11644473600` seconds
/// between the two epochs) is the standard, long-stable conversion, not
/// a heuristic.
pub fn read_chromium_history(path: &std::path::Path) -> Result<Vec<HistoryEntry>, String> {
    let conn = open_readonly(path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT url, COALESCE(title, ''), visit_count, last_visit_time FROM urls ORDER BY last_visit_time DESC",
        )
        .map_err(|e| e.to_string())?;

    const CHROMIUM_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

    let rows = stmt
        .query_map([], |row| {
            let url: String = row.get(0)?;
            let title: String = row.get(1)?;
            let visit_count: i64 = row.get(2)?;
            let chromium_micros: i64 = row.get(3)?;
            let last_visit_unix = if chromium_micros > 0 {
                (chromium_micros / 1_000_000) - CHROMIUM_EPOCH_OFFSET_SECS
            } else {
                0
            };
            Ok(HistoryEntry {
                url,
                title,
                visit_count,
                last_visit_unix,
            })
        })
        .map_err(|e| e.to_string())?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Reads a Firefox `places.sqlite` file. `moz_historyvisits.visit_date`
/// is microseconds since the UNIX epoch (unlike Chromium's WebKit-epoch
/// timestamps above), so this conversion is simpler.
pub fn read_firefox_history(path: &std::path::Path) -> Result<Vec<HistoryEntry>, String> {
    let conn = open_readonly(path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT p.url, COALESCE(p.title, ''), p.visit_count, COALESCE(MAX(h.visit_date), 0)
             FROM moz_places p LEFT JOIN moz_historyvisits h ON h.place_id = p.id
             GROUP BY p.id ORDER BY MAX(h.visit_date) DESC",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([], |row| {
            let url: String = row.get(0)?;
            let title: String = row.get(1)?;
            let visit_count: i64 = row.get(2)?;
            let visit_date_micros: i64 = row.get(3)?;
            Ok(HistoryEntry {
                url,
                title,
                visit_count,
                last_visit_unix: visit_date_micros / 1_000_000,
            })
        })
        .map_err(|e| e.to_string())?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Tries the Chromium schema first, then Firefox's, since both are
/// plain SQLite files and the caller may not know which browser produced
/// a given path (e.g. when scanning a profile directory for `*.sqlite`).
pub fn read_history_auto(path: &std::path::Path) -> Result<Vec<HistoryEntry>, String> {
    read_chromium_history(path).or_else(|_| read_firefox_history(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nexus-history-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn reads_chromium_style_history() {
        let path = temp_db("History");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT, visit_count INTEGER, last_visit_time INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO urls (url, title, visit_count, last_visit_time) VALUES ('https://example.com', 'Example Domain', 3, 13300000000000000)",
            [],
        )
        .unwrap();
        drop(conn);

        let entries = read_chromium_history(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "https://example.com");
        assert!(entries[0].last_visit_unix > 0);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn reads_firefox_style_history() {
        let path = temp_db("places.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT, title TEXT, visit_count INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE moz_historyvisits (id INTEGER PRIMARY KEY, place_id INTEGER, visit_date INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_places (url, title, visit_count) VALUES ('https://rust-lang.org', 'Rust', 5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_historyvisits (place_id, visit_date) VALUES (1, 1700000000000000)",
            [],
        )
        .unwrap();
        drop(conn);

        let entries = read_firefox_history(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "https://rust-lang.org");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
