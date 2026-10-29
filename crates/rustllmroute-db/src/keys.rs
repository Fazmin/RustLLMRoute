use sqlx::QueryBuilder;
use sqlx::Sqlite;

use crate::pool::{Page, Paged};
use crate::rows::KeyRow;
use crate::{Db, DbError, now_ms};

const COLUMNS: &str = "token, key_name, key_alias, user_id, team_id, spend, max_budget, soft_budget, budget_duration, budget_reset_at, expires_at, models, aliases, model_max_budget, allowed_routes, metadata, max_parallel_requests, tpm_limit, rpm_limit, blocked, created_at, updated_at, last_active_at, created_by";

/// Fields a `/key/update` may change. `Some(None)` clears a nullable column.
#[derive(Clone, Debug, Default)]
pub struct KeyPatch {
    pub key_alias: Option<Option<String>>,
    pub user_id: Option<Option<String>>,
    pub team_id: Option<Option<String>>,
    pub spend: Option<f64>,
    pub max_budget: Option<Option<f64>>,
    pub soft_budget: Option<Option<f64>>,
    pub budget_duration: Option<Option<String>>,
    pub budget_reset_at: Option<Option<i64>>,
    pub expires_at: Option<Option<i64>>,
    pub models: Option<String>,
    pub aliases: Option<String>,
    pub model_max_budget: Option<String>,
    pub allowed_routes: Option<String>,
    pub metadata: Option<String>,
    pub max_parallel_requests: Option<Option<i64>>,
    pub tpm_limit: Option<Option<i64>>,
    pub rpm_limit: Option<Option<i64>>,
    pub blocked: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct KeyFilter {
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub key_alias: Option<String>,
    pub include_blocked: bool,
}

fn push_filter(builder: &mut QueryBuilder<Sqlite>, filter: &KeyFilter) {
    builder.push(" WHERE 1 = 1");
    if let Some(user_id) = &filter.user_id {
        builder.push(" AND user_id = ").push_bind(user_id.clone());
    }
    if let Some(team_id) = &filter.team_id {
        builder.push(" AND team_id = ").push_bind(team_id.clone());
    }
    if let Some(alias) = &filter.key_alias {
        builder.push(" AND key_alias = ").push_bind(alias.clone());
    }
    if !filter.include_blocked {
        builder.push(" AND blocked = FALSE");
    }
}

impl Db {
    pub async fn get_key(&self, token: &str) -> Result<Option<KeyRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM keys WHERE token = $1");
        self.timed(
            sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql))
                .bind(token)
                .fetch_optional(self.reader()),
        )
        .await
    }

    pub async fn get_key_by_alias(&self, alias: &str) -> Result<Option<KeyRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM keys WHERE key_alias = $1");
        self.timed(
            sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql))
                .bind(alias)
                .fetch_optional(self.reader()),
        )
        .await
    }

    pub async fn insert_key(&self, row: &KeyRow) -> Result<(), DbError> {
        let sql = format!(
            "INSERT INTO keys ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24)"
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&row.token)
            .bind(&row.key_name)
            .bind(&row.key_alias)
            .bind(&row.user_id)
            .bind(&row.team_id)
            .bind(row.spend)
            .bind(row.max_budget)
            .bind(row.soft_budget)
            .bind(&row.budget_duration)
            .bind(row.budget_reset_at)
            .bind(row.expires_at)
            .bind(&row.models)
            .bind(&row.aliases)
            .bind(&row.model_max_budget)
            .bind(&row.allowed_routes)
            .bind(&row.metadata)
            .bind(row.max_parallel_requests)
            .bind(row.tpm_limit)
            .bind(row.rpm_limit)
            .bind(row.blocked)
            .bind(row.created_at)
            .bind(row.updated_at)
            .bind(row.last_active_at)
            .bind(&row.created_by)
            .execute(self.writer())
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) => Err(DbError::conflict_on(error, "key_alias")),
        }
    }

    /// Applies a patch; returns false when no key has that token.
    pub async fn update_key(&self, token: &str, patch: &KeyPatch) -> Result<bool, DbError> {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE keys SET updated_at = ");
        builder.push_bind(now_ms());
        macro_rules! set {
            ($field:ident) => {
                if let Some(value) = &patch.$field {
                    builder.push(concat!(", ", stringify!($field), " = "));
                    builder.push_bind(value.clone());
                }
            };
        }
        set!(key_alias);
        set!(user_id);
        set!(team_id);
        set!(spend);
        set!(max_budget);
        set!(soft_budget);
        set!(budget_duration);
        set!(budget_reset_at);
        set!(expires_at);
        set!(models);
        set!(aliases);
        set!(model_max_budget);
        set!(allowed_routes);
        set!(metadata);
        set!(max_parallel_requests);
        set!(tpm_limit);
        set!(rpm_limit);
        set!(blocked);
        builder.push(" WHERE token = ");
        builder.push_bind(token.to_owned());
        let result = builder.build().execute(self.writer()).await;
        match result {
            Ok(done) => Ok(done.rows_affected() > 0),
            Err(error) => Err(DbError::conflict_on(error, "key_alias")),
        }
    }

    pub async fn set_keys_blocked(&self, tokens: &[String], blocked: bool) -> Result<u64, DbError> {
        if tokens.is_empty() {
            return Ok(0);
        }
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE keys SET blocked = ");
        builder.push_bind(blocked);
        builder.push(", updated_at = ");
        builder.push_bind(now_ms());
        builder.push(" WHERE token IN (");
        let mut separated = builder.separated(", ");
        for token in tokens {
            separated.push_bind(token.clone());
        }
        separated.push_unseparated(")");
        Ok(builder
            .build()
            .execute(self.writer())
            .await?
            .rows_affected())
    }

    pub async fn delete_keys(&self, tokens: &[String]) -> Result<u64, DbError> {
        if tokens.is_empty() {
            return Ok(0);
        }
        let mut builder: QueryBuilder<Sqlite> =
            QueryBuilder::new("DELETE FROM keys WHERE token IN (");
        let mut separated = builder.separated(", ");
        for token in tokens {
            separated.push_bind(token.clone());
        }
        separated.push_unseparated(")");
        Ok(builder
            .build()
            .execute(self.writer())
            .await?
            .rows_affected())
    }

    pub async fn list_keys(
        &self,
        filter: &KeyFilter,
        page: Page,
    ) -> Result<Paged<KeyRow>, DbError> {
        let mut count: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT COUNT(*) FROM keys");
        push_filter(&mut count, filter);
        let total: i64 = count.build_query_scalar().fetch_one(self.reader()).await?;
        let mut select: QueryBuilder<Sqlite> =
            QueryBuilder::new(format!("SELECT {COLUMNS} FROM keys"));
        push_filter(&mut select, filter);
        select.push(" ORDER BY created_at DESC LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        let items = select
            .build_query_as::<KeyRow>()
            .fetch_all(self.reader())
            .await?;
        Ok(Paged {
            items,
            total,
            page: page.page,
            size: page.size,
        })
    }

    /// Key rotation: copies the row under the new token and deletes the old one, atomically.
    /// The alias moves last so the UNIQUE constraint never sees both rows holding it.
    pub async fn replace_key_token(
        &self,
        old_token: &str,
        new_token: &str,
        new_key_name: &str,
    ) -> Result<(), DbError> {
        let mut conn = self.writer().acquire().await?;
        let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE").await?;
        let alias: Option<Option<String>> =
            sqlx::query_scalar("SELECT key_alias FROM keys WHERE token = $1")
                .bind(old_token)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(alias) = alias else {
            return Err(DbError::NotFound);
        };
        let sql = format!(
            "INSERT INTO keys ({COLUMNS}) SELECT $1, $2, NULL, user_id, team_id, spend, max_budget, soft_budget, budget_duration, budget_reset_at, expires_at, models, aliases, model_max_budget, allowed_routes, metadata, max_parallel_requests, tpm_limit, rpm_limit, blocked, created_at, $3, last_active_at, created_by FROM keys WHERE token = $4"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(new_token)
            .bind(new_key_name)
            .bind(now_ms())
            .bind(old_token)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM keys WHERE token = $1")
            .bind(old_token)
            .execute(&mut *tx)
            .await?;
        if let Some(alias) = alias {
            sqlx::query("UPDATE keys SET key_alias = $1 WHERE token = $2")
                .bind(alias)
                .bind(new_token)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
