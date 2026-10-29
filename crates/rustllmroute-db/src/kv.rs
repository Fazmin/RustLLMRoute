use crate::{Db, DbError, now_ms};

impl Db {
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>, DbError> {
        Ok(
            sqlx::query_scalar::<_, String>("SELECT v FROM kv WHERE k = $1")
                .bind(key)
                .fetch_optional(self.reader())
                .await?,
        )
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO kv (k, v, updated_at) VALUES ($1, $2, $3) ON CONFLICT (k) DO UPDATE SET v = excluded.v, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_ms())
        .execute(self.writer())
        .await?;
        Ok(())
    }
}
