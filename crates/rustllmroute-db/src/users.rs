use sqlx::{QueryBuilder, Sqlite};

use crate::pool::{Page, Paged};
use crate::rows::UserRow;
use crate::{Db, DbError, now_ms};

const COLUMNS: &str = "user_id, user_email, user_alias, user_role, spend, max_budget, budget_duration, budget_reset_at, models, metadata, max_parallel_requests, tpm_limit, rpm_limit, created_at, updated_at";

#[derive(Clone, Debug, Default)]
pub struct UserPatch {
    pub user_email: Option<Option<String>>,
    pub user_alias: Option<Option<String>>,
    pub user_role: Option<String>,
    pub spend: Option<f64>,
    pub max_budget: Option<Option<f64>>,
    pub budget_duration: Option<Option<String>>,
    pub budget_reset_at: Option<Option<i64>>,
    pub models: Option<String>,
    pub metadata: Option<String>,
    pub max_parallel_requests: Option<Option<i64>>,
    pub tpm_limit: Option<Option<i64>>,
    pub rpm_limit: Option<Option<i64>>,
}

impl Db {
    pub async fn get_user(&self, user_id: &str) -> Result<Option<UserRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM users WHERE user_id = $1");
        self.timed(
            sqlx::query_as::<_, UserRow>(sqlx::AssertSqlSafe(sql))
                .bind(user_id)
                .fetch_optional(self.reader()),
        )
        .await
    }

    pub async fn insert_user(&self, row: &UserRow) -> Result<(), DbError> {
        let sql = format!(
            "INSERT INTO users ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&row.user_id)
            .bind(&row.user_email)
            .bind(&row.user_alias)
            .bind(&row.user_role)
            .bind(row.spend)
            .bind(row.max_budget)
            .bind(&row.budget_duration)
            .bind(row.budget_reset_at)
            .bind(&row.models)
            .bind(&row.metadata)
            .bind(row.max_parallel_requests)
            .bind(row.tpm_limit)
            .bind(row.rpm_limit)
            .bind(row.created_at)
            .bind(row.updated_at)
            .execute(self.writer())
            .await
            .map_err(|error| DbError::conflict_on(error, "user_id"))?;
        Ok(())
    }

    pub async fn update_user(&self, user_id: &str, patch: &UserPatch) -> Result<bool, DbError> {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE users SET updated_at = ");
        builder.push_bind(now_ms());
        macro_rules! set {
            ($field:ident) => {
                if let Some(value) = &patch.$field {
                    builder.push(concat!(", ", stringify!($field), " = "));
                    builder.push_bind(value.clone());
                }
            };
        }
        set!(user_email);
        set!(user_alias);
        set!(user_role);
        set!(spend);
        set!(max_budget);
        set!(budget_duration);
        set!(budget_reset_at);
        set!(models);
        set!(metadata);
        set!(max_parallel_requests);
        set!(tpm_limit);
        set!(rpm_limit);
        builder.push(" WHERE user_id = ");
        builder.push_bind(user_id.to_owned());
        let done = builder
            .build()
            .execute(self.writer())
            .await
            .map_err(|error| DbError::conflict_on(error, "user_email"))?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn delete_users(&self, user_ids: &[String]) -> Result<u64, DbError> {
        if user_ids.is_empty() {
            return Ok(0);
        }
        let mut builder: QueryBuilder<Sqlite> =
            QueryBuilder::new("DELETE FROM users WHERE user_id IN (");
        let mut separated = builder.separated(", ");
        for id in user_ids {
            separated.push_bind(id.clone());
        }
        separated.push_unseparated(")");
        Ok(builder
            .build()
            .execute(self.writer())
            .await?
            .rows_affected())
    }

    pub async fn list_users(&self, page: Page) -> Result<Paged<UserRow>, DbError> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(self.reader())
            .await?;
        let sql =
            format!("SELECT {COLUMNS} FROM users ORDER BY created_at DESC LIMIT $1 OFFSET $2");
        let items = sqlx::query_as::<_, UserRow>(sqlx::AssertSqlSafe(sql))
            .bind(page.limit())
            .bind(page.offset())
            .fetch_all(self.reader())
            .await?;
        Ok(Paged {
            items,
            total,
            page: page.page,
            size: page.size,
        })
    }
}
