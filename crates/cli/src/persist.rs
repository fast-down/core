use crate::progress;
use color_eyre::eyre::Result;
use fast_down::Progress;
use rusqlite::{Connection, ErrorCode, OptionalExtension};
use std::{env, path::Path};

pub struct WriteProgress {
    pub total_size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub progress: Vec<Progress>,
}

pub fn init_db() -> Result<Connection> {
    let self_path = env::current_exe()?;
    let db_path = self_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("state.db");
    let conn = match Connection::open(&db_path) {
        Ok(conn) => conn,
        Err(e) => {
            if e.sqlite_error_code() != Some(ErrorCode::CannotOpen) {
                Err(e)?;
            }
            println!("无法打开 {}, 将在当前工作目录创建数据库", db_path.display());
            Connection::open("./fast-down.db")?
        }
    };
    conn.execute(
        "CREATE TABLE IF NOT EXISTS write_progress (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL UNIQUE,
            total_size INTEGER NOT NULL,
            etag TEXT,
            last_modified TEXT,
            progress TEXT NOT NULL
        )",
        (),
    )?;
    Ok(conn)
}

pub fn init_progress(
    conn: &Connection,
    file_path: &str,
    total_size: u64,
    etag: Option<String>,
    last_modified: Option<String>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO write_progress
        (file_path, total_size, etag, last_modified, progress)
        VALUES (?1, ?2, ?3, ?4, ?5)",
        (file_path, total_size, etag, last_modified, ""),
    )?;
    Ok(())
}

pub fn update_progress(conn: &Connection, file_path: &str, progress: &[Progress]) -> Result<()> {
    conn.execute(
        "UPDATE write_progress SET progress = ?1 WHERE file_path = ?2",
        (progress::to_string(progress), file_path),
    )?;
    Ok(())
}

pub fn get_progress(conn: &Connection, file_path: &str) -> Result<Option<WriteProgress>> {
    conn.query_row(
        "SELECT total_size, etag, last_modified, progress
        FROM write_progress WHERE file_path = ?1",
        [file_path],
        |row| {
            let progress: String = row.get(3)?;
            Ok(WriteProgress {
                total_size: row.get(0)?,
                etag: row.get(1)?,
                last_modified: row.get(2)?,
                progress: progress::from_str(&progress),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
