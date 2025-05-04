use color_eyre::eyre::Result;
use fast_down::{fmt_progress, Progress};
use rusqlite::{Connection, OptionalExtension};

use crate::str_to_progress::str_to_progress;

pub struct WriteProgress {
    pub total_size: usize,
    pub progress: Vec<Progress>,
}

pub fn init_db(db_path: &str) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS write_progress (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL UNIQUE,
            total_size INTEGER NOT NULL,
            progress TEXT NOT NULL
        )",
        (),
    )?;
    Ok(conn)
}

pub fn init_progress(conn: &Connection, file_path: &str, total_size: usize) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO write_progress
        (file_path, total_size, progress)
        VALUES (?1, ?2, ?3)",
        (file_path, total_size, ""),
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
        "SELECT total_size, progress
        FROM write_progress WHERE file_path = ?1",
        [file_path],
        |row| {
            Ok(WriteProgress {
                total_size: row.get(0)?,
                progress: str_to_progress(row.get(1)?),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
