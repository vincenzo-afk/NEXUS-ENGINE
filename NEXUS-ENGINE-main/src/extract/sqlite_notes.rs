//! Generic SQLite extraction for note-taking apps that store notes in a
//! local `.sqlite`/`.db` file with an unknown/app-specific schema
//! (Notes-style apps, some journaling apps, etc). Rather than hard-coding
//! one app's table layout, this walks every user table and concatenates
//! every `TEXT` column's values — a note title/body column always shows
//! up as *some* text column, so this is a robust if slightly noisy
//! extractor. For a schema Nexus knows specifically (browser history),
//! see [`crate::extract::browser_history`] instead, which targets exact
//! tables/columns and returns structured rows rather than a text blob.

use rusqlite::Connection;

/// Opens `path` read-only and concatenates every `TEXT`/`VARCHAR` column
/// value from every user table (`sqlite_master` entries not prefixed
/// `sqlite_`) into one indexable blob, one row's values per line.
/// Returns an empty string (not an error) for anything that isn't a
/// readable SQLite database, consistent with the rest of `extract`'s
/// best-effort philosophy — a locked or malformed `.db` file should not
/// abort a bulk index run.
pub fn extract_text(path: &std::path::Path) -> super::ExtractedText {
    let conn = match Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => return super::ExtractedText::empty_with_warning(format!("cannot open: {e}")),
    };

    let table_names: Vec<String> = match conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    ) {
        Ok(mut stmt) => stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(e) => return super::ExtractedText::empty_with_warning(format!("cannot list tables: {e}")),
    };

    if table_names.is_empty() {
        return super::ExtractedText::empty_with_warning("no user tables found");
    }

    let mut out = String::new();
    for table in &table_names {
        let text_columns = text_columns_of(&conn, table);
        if text_columns.is_empty() {
            continue;
        }
        // Table/column names come from sqlite_master/pragma output, not
        // caller input, so building the SQL by string interpolation here
        // does not create a caller-controlled injection point.
        let cols = text_columns.join(", ");
        let sql = format!("SELECT {cols} FROM \"{table}\"");
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let n = text_columns.len();
        let rows = stmt.query_map([], |row| {
            let mut vals = Vec::with_capacity(n);
            for i in 0..n {
                vals.push(row.get::<_, Option<String>>(i).unwrap_or(None).unwrap_or_default());
            }
            Ok(vals.join(" "))
        });
        if let Ok(rows) = rows {
            for row in rows.filter_map(|r| r.ok()) {
                if !row.trim().is_empty() {
                    out.push_str(&row);
                    out.push('\n');
                }
            }
        }
    }

    if out.trim().is_empty() {
        super::ExtractedText::empty_with_warning("no text-typed column values found")
    } else {
        super::ExtractedText::ok(out)
    }
}

fn text_columns_of(conn: &Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info(\"{table}\")");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |row| {
        let name: String = row.get(1)?;
        let type_name: String = row.get(2)?;
        Ok((name, type_name.to_uppercase()))
    })
    .map(|rows| {
        rows.filter_map(|r| r.ok())
            .filter(|(_, ty)| ty.contains("TEXT") || ty.contains("CHAR") || ty.contains("CLOB"))
            .map(|(name, _)| name)
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_columns_from_a_simple_notes_db() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-sqlite-notes-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT, body TEXT, pinned INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO notes (title, body, pinned) VALUES ('Grocery list', 'milk, eggs, bread', 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let result = extract_text(&path);
        assert!(result.text.contains("Grocery list"));
        assert!(result.text.contains("milk"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_sqlite_file_degrades_to_warning() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-sqlite-notes-bad-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not_a_db.sqlite");
        std::fs::write(&path, b"definitely not a sqlite file").unwrap();
        let result = extract_text(&path);
        assert!(result.text.is_empty());
        assert!(result.warning.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
