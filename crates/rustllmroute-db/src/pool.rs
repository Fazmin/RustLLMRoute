use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

use crate::DbError;

#[derive(Clone, Debug)]
pub struct DbOptions {
    pub reader_connections: u32,
    pub synchronous_full: bool,
    pub query_timeout: Duration,
    pub busy_timeout: Duration,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            reader_connections: 4,
            synchronous_full: false,
            query_timeout: Duration::from_secs(10),
            busy_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub page: u32,
    pub size: u32,
}

impl Page {
    pub fn new(page: u32, size: u32) -> Self {
        Self {
            page: page.max(1),
            size: size.clamp(1, 1000),
        }
    }

    pub fn offset(&self) -> i64 {
        i64::from(self.page - 1) * i64::from(self.size)
    }

    pub fn limit(&self) -> i64 {
        i64::from(self.size)
    }
}

#[derive(Clone, Debug)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub size: u32,
}

impl<T> Paged<T> {
    pub fn total_pages(&self) -> i64 {
        if self.size == 0 {
            return 0;
        }
        (self.total + i64::from(self.size) - 1) / i64::from(self.size)
    }
}

/// SQLite with WAL: exactly one writer connection (so our own writes never contend) and a small
/// read-only pool for queries. Every write transaction starts with `BEGIN IMMEDIATE` so the lock
/// is taken up front.
#[derive(Clone)]
pub struct Db {
    writer: SqlitePool,
    reader: SqlitePool,
    timeout: Duration,
}

impl Db {
    pub async fn connect(url: &str, options: DbOptions) -> Result<Self, DbError> {
        let synchronous = if options.synchronous_full {
            SqliteSynchronous::Full
        } else {
            SqliteSynchronous::Normal
        };
        let base = SqliteConnectOptions::from_str(url)
            .map_err(|error| DbError::Unavailable(format!("invalid sqlite url {url:?}: {error}")))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(synchronous)
            .busy_timeout(options.busy_timeout)
            .foreign_keys(true)
            .pragma("journal_size_limit", "67108864")
            .pragma("temp_store", "memory")
            .statement_cache_capacity(200);
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(base.clone())
            .await?;
        let reader = SqlitePoolOptions::new()
            .max_connections(options.reader_connections.max(1))
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(base.read_only(true))
            .await?;
        Ok(Self {
            writer,
            reader,
            timeout: options.query_timeout,
        })
    }

    pub async fn migrate(&self) -> Result<(), DbError> {
        sqlx::migrate!("../../migrations").run(&self.writer).await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        self.timed(async { sqlx::query("SELECT 1").execute(&self.reader).await })
            .await?;
        Ok(())
    }

    pub async fn checkpoint(&self) -> Result<(), DbError> {
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    pub async fn close(&self) {
        self.writer.close().await;
        self.reader.close().await;
    }

    pub(crate) fn writer(&self) -> &SqlitePool {
        &self.writer
    }

    pub(crate) fn reader(&self) -> &SqlitePool {
        &self.reader
    }

    pub(crate) async fn timed<T, F>(&self, future: F) -> Result<T, DbError>
    where
        F: Future<Output = Result<T, sqlx::Error>>,
    {
        match tokio::time::timeout(self.timeout, future).await {
            Ok(result) => result.map_err(DbError::from),
            Err(_) => Err(DbError::Timeout),
        }
    }
}
