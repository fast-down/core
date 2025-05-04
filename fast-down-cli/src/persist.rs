use color_eyre::eyre::Result;
use fast_down::{fmt_progress, Progress};
use rusqlite::{Connection, OptionalExtension};

use crate::str_to_progress::str_to_progress;

pub struct WriteProgress {
    pub url: String,
    pub file_path: String,
    pub total_size: usize,
    pub downloaded: usize,
    pub progress: Vec<Progress>,
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
            progress TEXT NOT NULL
        )",
        (),
    )?;
    Ok(conn)
}

pub fn init_progress(conn: &Connection, progress: &WriteProgress) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO write_progress
        (url, file_path, total_size, downloaded, progress)
        VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            &progress.url,
            &progress.file_path,
            progress.total_size as i64,
            progress.downloaded as i64,
            fmt_progress(&progress.progress),
        ),
    )?;
    Ok(())
}

pub fn update_progress(conn: &Connection, file_path: &str, progress: &[Progress]) -> Result<()> {
    conn.execute(
        "UPDATE write_progress SET progress = ?1 WHERE file_path = ?2",
        (fmt_progress(progress), file_path),
    )?;
    Ok(())
}

pub fn get_progress(conn: &Connection, file_path: &str) -> Result<Option<WriteProgress>> {
    conn.query_row(
        "SELECT url, file_path, total_size, downloaded, progress
        FROM write_progress WHERE file_path = ?1",
        [file_path],
        |row| {
            Ok(WriteProgress {
                url: row.get(0)?,
                file_path: row.get(1)?,
                total_size: row.get(2)?,
                downloaded: row.get(3)?,
                progress: str_to_progress(row.get(4)?),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
