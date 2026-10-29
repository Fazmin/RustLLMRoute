#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("{field} already exists")]
    Conflict { field: &'static str },
    #[error("not found")]
    NotFound,
    #[error("database is busy")]
    Busy,
    #[error("database disk is full")]
    DiskFull,
    #[error("database unavailable: {0}")]
    Unavailable(String),
    #[error("database query timed out")]
    Timeout,
    #[error("database error: {0}")]
    Other(#[source] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

impl DbError {
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Busy | Self::Unavailable(_) | Self::Timeout | Self::DiskFull
        )
    }

    pub(crate) fn conflict_on(error: sqlx::Error, field: &'static str) -> Self {
        match &error {
            sqlx::Error::Database(db) if db.is_unique_violation() => Self::Conflict { field },
            _ => Self::from(error),
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::RowNotFound => Self::NotFound,
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => {
                Self::Unavailable(error.to_string())
            }
            sqlx::Error::Io(_) => Self::Unavailable(error.to_string()),
            sqlx::Error::Database(db) => {
                let code = db.code().map(|c| c.to_string()).unwrap_or_default();
                match code.as_str() {
                    "5" | "6" | "261" | "262" | "517" => Self::Busy,
                    "13" | "53100" => Self::DiskFull,
                    _ if db.is_unique_violation() => Self::Conflict { field: "id" },
                    _ => Self::Other(error),
                }
            }
            _ => Self::Other(error),
        }
    }
}
