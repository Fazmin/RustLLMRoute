use crate::{Db, DbError, now_ms};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetEntity {
    Key,
    Team,
    User,
}

impl BudgetEntity {
    fn table(self) -> &'static str {
        match self {
            Self::Key => "keys",
            Self::Team => "teams",
            Self::User => "users",
        }
    }

    fn id_column(self) -> &'static str {
        match self {
            Self::Key => "token",
            Self::Team => "team_id",
            Self::User => "user_id",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Team => "team",
            Self::User => "user",
        }
    }
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct BudgetDue {
    pub id: String,
    pub budget_duration: String,
    pub budget_reset_at: i64,
}

impl Db {
    pub async fn budgets_due(
        &self,
        entity: BudgetEntity,
        now: i64,
        limit: u32,
    ) -> Result<Vec<BudgetDue>, DbError> {
        let sql = format!(
            "SELECT {} AS id, budget_duration, budget_reset_at FROM {} WHERE budget_reset_at IS NOT NULL AND budget_duration IS NOT NULL AND budget_reset_at <= $1 ORDER BY budget_reset_at ASC LIMIT $2",
            entity.id_column(),
            entity.table()
        );
        Ok(sqlx::query_as::<_, BudgetDue>(sqlx::AssertSqlSafe(sql))
            .bind(now)
            .bind(i64::from(limit))
            .fetch_all(self.reader())
            .await?)
    }

    /// Zeroes spend and advances the reset time. The `old_reset_at` predicate makes a repeated run
    /// (two instances, a retry) a no-op instead of a double reset.
    pub async fn reset_budgets(
        &self,
        entity: BudgetEntity,
        resets: &[(String, i64, i64)],
    ) -> Result<u64, DbError> {
        if resets.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "UPDATE {} SET spend = 0, budget_reset_at = $1, updated_at = $2 WHERE {} = $3 AND budget_reset_at = $4",
            entity.table(),
            entity.id_column()
        );
        let now = now_ms();
        let mut conn = self.writer().acquire().await?;
        let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE").await?;
        let mut updated = 0;
        for (id, old_reset_at, new_reset_at) in resets {
            updated += sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(new_reset_at)
                .bind(now)
                .bind(id)
                .bind(old_reset_at)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        Ok(updated)
    }
}
