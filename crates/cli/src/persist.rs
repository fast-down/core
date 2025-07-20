use crate::progress;
use color_eyre::Result;
use fast_down::ProgressEntry;
use std::{env, path::Path, sync::Arc};
use tokio_rusqlite::{Connection, ErrorCode, OptionalExtension};

#[derive(Debug, Clone)]
pub struct DatabaseEntry {
    pub total_size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub progress: Vec<ProgressEntry>,
    #[allow(dead_code)]
    pub file_name: String,
    #[allow(dead_code)]
    pub elapsed: u64,
    #[allow(dead_code)]
    pub url: String,
}

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub async fn new() -> Result<Self> {
        let self_path = env::current_exe()?;
        let db_path = self_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("state.db");
        let conn = match Connection::open(&db_path).await {
            Ok(conn) => conn,
            Err(e) => match e {
                tokio_rusqlite::Error::Rusqlite(e) => {
                    if e.sqlite_error_code() != Some(ErrorCode::CannotOpen) {
                        Err(e)?;
                    }
                    println!(
                        "无法打开 {}, 尝试在当前工作目录创建数据库",
                        db_path.display()
                    );
                    Connection::open("./fast-down.db").await?
                }
                _ => return Err(e.into()),
            },
        };
        conn.call(|conn| {
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
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn init_entry(
        &self,
        file_path: Arc<String>,
        total_size: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        file_name: String,
        url: String,
    ) -> Result<usize, tokio_rusqlite::Error> {
        self.conn
            .call(move |conn| {
                conn.execute(
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
                )
                .map_err(Into::into)
            })
            .await
    }

    pub async fn get_entry(
        &self,
        file_path: Arc<String>,
    ) -> Result<Option<DatabaseEntry>, tokio_rusqlite::Error> {
        self.conn
            .call(|conn| {
                conn.query_row(
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
            })
            .await
    }

    pub async fn update_entry(
        &self,
        file_path: Arc<String>,
        progress: Vec<ProgressEntry>,
        elapsed: u64,
    ) -> Result<usize, tokio_rusqlite::Error> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE write_progress SET progress = ?1, elapsed = ?2 WHERE file_path = ?3",
                    (progress::to_string(&progress), elapsed, file_path),
                )
                .map_err(Into::into)
            })
            .await
    }

    pub async fn clean(&self) -> Result<usize, tokio_rusqlite::Error> {
        self.conn
            .call(|conn| {
                conn.execute(
                    "DELETE FROM write_progress WHERE progress = '0-' || (total_size - 1)",
                    [],
                )
                .map_err(Into::into)
            })
            .await
    }
}
