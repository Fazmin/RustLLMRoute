use sqlx::{QueryBuilder, Sqlite};

use crate::pool::{Page, Paged};
use crate::rows::TeamRow;
use crate::{Db, DbError, now_ms};

const COLUMNS: &str = "team_id, team_alias, members, spend, max_budget, budget_duration, budget_reset_at, models, model_max_budget, metadata, max_parallel_requests, tpm_limit, rpm_limit, blocked, created_at, updated_at";

#[derive(Clone, Debug, Default)]
pub struct TeamPatch {
    pub team_alias: Option<Option<String>>,
    pub members: Option<String>,
    pub spend: Option<f64>,
    pub max_budget: Option<Option<f64>>,
    pub budget_duration: Option<Option<String>>,
    pub budget_reset_at: Option<Option<i64>>,
    pub models: Option<String>,
    pub model_max_budget: Option<String>,
    pub metadata: Option<String>,
    pub max_parallel_requests: Option<Option<i64>>,
    pub tpm_limit: Option<Option<i64>>,
    pub rpm_limit: Option<Option<i64>>,
    pub blocked: Option<bool>,
}

impl Db {
    pub async fn get_team(&self, team_id: &str) -> Result<Option<TeamRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM teams WHERE team_id = $1");
        self.timed(
            sqlx::query_as::<_, TeamRow>(sqlx::AssertSqlSafe(sql))
                .bind(team_id)
                .fetch_optional(self.reader()),
        )
        .await
    }

    pub async fn insert_team(&self, row: &TeamRow) -> Result<(), DbError> {
        let sql = format!(
            "INSERT INTO teams ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&row.team_id)
            .bind(&row.team_alias)
            .bind(&row.members)
            .bind(row.spend)
            .bind(row.max_budget)
            .bind(&row.budget_duration)
            .bind(row.budget_reset_at)
            .bind(&row.models)
            .bind(&row.model_max_budget)
            .bind(&row.metadata)
            .bind(row.max_parallel_requests)
            .bind(row.tpm_limit)
            .bind(row.rpm_limit)
            .bind(row.blocked)
            .bind(row.created_at)
            .bind(row.updated_at)
            .execute(self.writer())
            .await
            .map_err(|error| DbError::conflict_on(error, "team_id"))?;
        Ok(())
    }

    pub async fn update_team(&self, team_id: &str, patch: &TeamPatch) -> Result<bool, DbError> {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE teams SET updated_at = ");
        builder.push_bind(now_ms());
        macro_rules! set {
            ($field:ident) => {
                if let Some(value) = &patch.$field {
                    builder.push(concat!(", ", stringify!($field), " = "));
                    builder.push_bind(value.clone());
                }
            };
        }
        set!(team_alias);
        set!(members);
        set!(spend);
        set!(max_budget);
        set!(budget_duration);
        set!(budget_reset_at);
        set!(models);
        set!(model_max_budget);
        set!(metadata);
        set!(max_parallel_requests);
        set!(tpm_limit);
        set!(rpm_limit);
        set!(blocked);
        builder.push(" WHERE team_id = ");
        builder.push_bind(team_id.to_owned());
        let done = builder
            .build()
            .execute(self.writer())
            .await
            .map_err(|error| DbError::conflict_on(error, "team_alias"))?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn delete_teams(&self, team_ids: &[String]) -> Result<u64, DbError> {
        if team_ids.is_empty() {
            return Ok(0);
        }
        let mut builder: QueryBuilder<Sqlite> =
            QueryBuilder::new("DELETE FROM teams WHERE team_id IN (");
        let mut separated = builder.separated(", ");
        for id in team_ids {
            separated.push_bind(id.clone());
        }
        separated.push_unseparated(")");
        Ok(builder
            .build()
            .execute(self.writer())
            .await?
            .rows_affected())
    }

    pub async fn list_teams(&self, page: Page) -> Result<Paged<TeamRow>, DbError> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM teams")
            .fetch_one(self.reader())
            .await?;
        let sql =
            format!("SELECT {COLUMNS} FROM teams ORDER BY created_at DESC LIMIT $1 OFFSET $2");
        let items = sqlx::query_as::<_, TeamRow>(sqlx::AssertSqlSafe(sql))
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
