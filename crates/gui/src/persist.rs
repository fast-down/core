use crate::progress;
use color_eyre::eyre::Result;
use fast_down::ProgressEntry;
use rusqlite::{Connection, ErrorCode, OptionalExtension};
use std::{env, path::Path};

#[derive(Clone)]
pub struct DatabaseEntry {
    pub total_size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub progress: Vec<ProgressEntry>,
    pub file_name: String,
    pub elapsed: u64,
    pub url: String,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new() -> Result<Self> {
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
                progress TEXT NOT NULL,
                file_name TEXT NOT NULL,
                elapsed INTEGER NOT NULL,
                url TEXT NOT NULL
            )",
            (),
        )?;
        Ok(Self { conn })
    }

    pub fn init_progress(
        &self,
        file_path: &str,
        total_size: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        file_name: &str,
        url: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO write_progress
            (file_path, total_size, etag, last_modified, progress, file_name, elapsed, url)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                file_path,
                total_size,
                etag,
                last_modified,
                "",
                file_name,
                0,
                url,
            ),
        )?;
        Ok(())
    }

    pub fn get_progress(&self, file_path: &str) -> Result<Option<DatabaseEntry>> {
        self.conn
            .query_row(
                "SELECT total_size, etag, last_modified, progress, file_name, elapsed, url
            FROM write_progress WHERE file_path = ?1",
                [file_path],
                |row| {
                    let progress: String = row.get(3)?;
                    Ok(DatabaseEntry {
                        total_size: row.get(0)?,
                        etag: row.get(1)?,
                        last_modified: row.get(2)?,
                        progress: progress::from_str(&progress),
                        file_name: row.get(4)?,
                        elapsed: row.get(5)?,
                        url: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn update_progress(
        &self,
        file_path: &str,
        progress: &[ProgressEntry],
        elapsed: u64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE write_progress SET progress = ?1, elapsed = ?2 WHERE file_path = ?3",
            (progress::to_string(progress), elapsed, file_path),
        )?;
        Ok(())
    }

    pub fn get_all_progress(&self) -> Result<Vec<DatabaseEntry>> {
        Ok(self.conn
            .prepare("SELECT total_size, etag, last_modified, progress, file_name, elapsed, url FROM write_progress")?
            .query_map([], |row| {
                let progress: String = row.get(3)?;
                Ok(DatabaseEntry {
                    total_size: row.get(0)?,
                    etag: row.get(1)?,
                    last_modified: row.get(2)?,
                    progress: progress::from_str(&progress),
                    file_name: row.get(4)?,
                    elapsed: row.get(5)?,
                    url: row.get(6)?,
                })
            })?
            .filter(|row| row.is_ok())
            .map(|row| row.unwrap())
            .collect())
    }

    pub fn clean(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM write_progress WHERE progress = '0-' || (total_size - 1)",
            [],
        )?)
    }
}
