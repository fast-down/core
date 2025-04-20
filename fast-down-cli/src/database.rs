use color_eyre::eyre::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Serialize)]
pub struct WriteProgress {
    url: String,
    file_path: String,
    total_size: usize,
    downloaded: usize,
    timestamp: i64,
}

pub fn init_db(db_path: &str) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS write_progress (
            id INTEGER PRIMARY KEY,
            url TEXT NOT NULL,
            file_path TEXT NOT NULL UNIQUE,
            total_size INTEGER NOT NULL,
            downloaded INTEGER NOT NULL,
            timestamp INTEGER NOT NULL
        )",
        [],
    )?;
    Ok(conn)
}

pub fn save_progress(conn: &Connection, progress: &WriteProgress) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO write_progress 
        (url, file_path, total_size, downloaded, timestamp)
        VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            progress.url,
            progress.file_path,
            progress.total_size as i64,
            progress.downloaded as i64,
            progress.timestamp
        ],
    )?;
    Ok(())
}

pub fn get_progress(conn: &Connection, file_path: &str) -> Result<Option<WriteProgress>> {
    conn.query_row(
        "SELECT url, file_path, total_size, downloaded, timestamp 
        FROM write_progress WHERE file_path = ?1",
        [file_path],
        |row| {
            Ok(WriteProgress {
                url: row.get(0)?,
                file_path: row.get(1)?,
                total_size: row.get(2)?,
                downloaded: row.get(3)?,
                timestamp: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}