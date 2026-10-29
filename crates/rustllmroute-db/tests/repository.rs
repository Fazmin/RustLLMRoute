use std::collections::BTreeMap;

use rustllmroute_db::{
    BudgetEntity, DailyDelta, DailyKey, Db, DbError, DbOptions, KeyFilter, KeyPatch, KeyRow, Page,
    SpendBatch, SpendGroupBy, SpendLogFilter, SpendLogRow, SpendRange, TeamRow, UserRow, day_of,
    now_ms,
};

async fn fresh() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("test.db")
        .display()
        .to_string()
        .replace('\\', "/");
    let db = Db::connect(&format!("sqlite://{path}"), DbOptions::default())
        .await
        .unwrap();
    db.migrate().await.unwrap();
    (dir, db)
}

fn key(token: &str, alias: Option<&str>) -> KeyRow {
    let mut row = KeyRow::new(token.to_owned(), now_ms());
    row.key_alias = alias.map(str::to_owned);
    row.key_name = Some(format!("sk-...{}", &token[token.len() - 4..]));
    row.models = r#"["gpt-4o"]"#.to_owned();
    row
}

fn log(request_id: &str, api_key: &str, spend: f64, start: i64) -> SpendLogRow {
    SpendLogRow {
        request_id: request_id.to_owned(),
        call_type: "chat_completion".to_owned(),
        api_key: api_key.to_owned(),
        key_alias: None,
        user_id: None,
        team_id: None,
        end_user: None,
        spend,
        total_tokens: 15,
        prompt_tokens: 10,
        completion_tokens: 5,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        start_time: start,
        end_time: start + 100,
        completion_start_time: None,
        request_duration_ms: Some(100),
        model: "gpt-4o".to_owned(),
        model_id: Some("dep-1".to_owned()),
        model_group: Some("gpt-4o".to_owned()),
        custom_llm_provider: Some("openai".to_owned()),
        api_base: None,
        status: "success".to_owned(),
        error: None,
        cache_hit: false,
        stream: false,
        partial: false,
        request_tags: "[]".to_owned(),
        metadata: "{}".to_owned(),
        request_body: None,
        response_body: None,
        session_id: None,
        litellm_call_id: Some(request_id.to_owned()),
        created_at: start,
    }
}

#[tokio::test]
async fn keys_round_trip_patch_and_conflicts() {
    let (_dir, db) = fresh().await;
    db.insert_key(&key("hash-a", Some("alpha"))).await.unwrap();
    assert!(matches!(
        db.insert_key(&key("hash-b", Some("alpha"))).await,
        Err(DbError::Conflict { field: "key_alias" })
    ));
    db.insert_key(&key("hash-b", None)).await.unwrap();
    let fetched = db.get_key("hash-a").await.unwrap().unwrap();
    assert_eq!(fetched.models_list(), vec!["gpt-4o"]);
    assert_eq!(
        db.get_key_by_alias("alpha").await.unwrap().unwrap().token,
        "hash-a"
    );
    let patch = KeyPatch {
        max_budget: Some(Some(12.5)),
        key_alias: Some(None),
        blocked: Some(true),
        ..KeyPatch::default()
    };
    assert!(db.update_key("hash-a", &patch).await.unwrap());
    assert!(!db.update_key("missing", &patch).await.unwrap());
    let updated = db.get_key("hash-a").await.unwrap().unwrap();
    assert_eq!(updated.max_budget, Some(12.5));
    assert_eq!(updated.key_alias, None);
    assert!(updated.blocked);
    let page = db
        .list_keys(&KeyFilter::default(), Page::new(1, 10))
        .await
        .unwrap();
    assert_eq!(page.total, 1, "blocked keys are hidden by default");
    let all = db
        .list_keys(
            &KeyFilter {
                include_blocked: true,
                ..KeyFilter::default()
            },
            Page::new(1, 1),
        )
        .await
        .unwrap();
    assert_eq!((all.total, all.items.len(), all.total_pages()), (2, 1, 2));
    db.replace_key_token("hash-b", "hash-c", "sk-...sh-c")
        .await
        .unwrap();
    assert!(db.get_key("hash-b").await.unwrap().is_none());
    assert_eq!(
        db.get_key("hash-c")
            .await
            .unwrap()
            .unwrap()
            .key_name
            .as_deref(),
        Some("sk-...sh-c")
    );
    assert!(matches!(
        db.replace_key_token("nope", "x", "y").await,
        Err(DbError::NotFound)
    ));
    assert_eq!(
        db.delete_keys(&["hash-a".to_owned(), "hash-c".to_owned()])
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn spend_batches_are_idempotent_and_aggregate() {
    let (_dir, db) = fresh().await;
    db.insert_key(&key("hash-a", None)).await.unwrap();
    db.insert_team(&TeamRow::new("team-1".to_owned(), now_ms()))
        .await
        .unwrap();
    db.insert_user(&UserRow::new("user-1".to_owned(), now_ms()))
        .await
        .unwrap();
    let start = 1_700_000_000_000;
    let daily_key = DailyKey {
        day: day_of(start),
        api_key: "hash-a".to_owned(),
        model: "gpt-4o".to_owned(),
        custom_llm_provider: "openai".to_owned(),
        model_group: "gpt-4o".to_owned(),
        team_id: "team-1".to_owned(),
        user_id: "user-1".to_owned(),
    };
    let mut batch = SpendBatch {
        logs: vec![
            log("r1", "hash-a", 0.5, start),
            log("r2", "hash-a", 0.25, start + 1),
        ],
        ..SpendBatch::default()
    };
    batch.key_delta.insert("hash-a".to_owned(), 0.75);
    batch.team_delta.insert("team-1".to_owned(), 0.75);
    batch.user_delta.insert("user-1".to_owned(), 0.75);
    batch.daily.insert(
        daily_key.clone(),
        DailyDelta {
            prompt_tokens: 20,
            completion_tokens: 10,
            spend: 0.75,
            api_requests: 2,
            successful_requests: 2,
            ..DailyDelta::default()
        },
    );
    batch.last_active.insert("hash-a".to_owned(), start + 1);
    let first = db.apply_spend_batch(&batch).await.unwrap();
    assert_eq!(
        (first.logs_written, first.entities_updated, first.daily_rows),
        (2, 3, 1)
    );
    let again = db.apply_spend_batch(&batch).await.unwrap();
    assert_eq!(again.logs_written, 0, "duplicate request ids are ignored");
    let fetched = db.get_key("hash-a").await.unwrap().unwrap();
    assert!(
        (fetched.spend - 1.5).abs() < 1e-9,
        "counters do add up on replay; the writer must not replay"
    );
    assert_eq!(fetched.last_active_at, Some(start + 1));
    assert!((db.get_team("team-1").await.unwrap().unwrap().spend - 1.5).abs() < 1e-9);
    let range = SpendRange {
        start_day: day_of(start),
        end_day: day_of(start),
        api_key: None,
        team_id: None,
        user_id: None,
        model: None,
    };
    let by_model = db.spend_summary(&range, SpendGroupBy::Model).await.unwrap();
    assert_eq!(by_model.len(), 1);
    assert_eq!(by_model[0].key, "gpt-4o");
    assert!((by_model[0].spend - 1.5).abs() < 1e-9);
    assert_eq!(by_model[0].total_tokens, 60);
    assert_eq!(by_model[0].requests, 4);
    let daily = db.daily_activity(&range).await.unwrap();
    assert_eq!(daily.len(), 1);
    assert_eq!(daily[0].successful_requests, 4);
    assert!((db.global_spend().await.unwrap() - 1.5).abs() < 1e-9);
    let logs = db
        .query_spend_logs(
            &SpendLogFilter {
                api_key: Some("hash-a".to_owned()),
                ..SpendLogFilter::default()
            },
            Page::new(1, 1),
        )
        .await
        .unwrap();
    assert_eq!((logs.total, logs.items.len()), (2, 1));
    assert_eq!(logs.items[0].request_id, "r2", "newest first");
    assert_eq!(db.delete_spend_logs_before(start + 1, 10).await.unwrap(), 1);
    assert_eq!(
        db.query_spend_logs(&SpendLogFilter::default(), Page::new(1, 10))
            .await
            .unwrap()
            .total,
        1
    );
}

#[tokio::test]
async fn budget_resets_are_idempotent() {
    let (_dir, db) = fresh().await;
    let mut row = key("hash-a", None);
    row.spend = 3.0;
    row.budget_duration = Some("1d".to_owned());
    row.budget_reset_at = Some(1_000);
    db.insert_key(&row).await.unwrap();
    let due = db.budgets_due(BudgetEntity::Key, 2_000, 100).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].budget_duration, "1d");
    let resets = vec![(due[0].id.clone(), due[0].budget_reset_at, 90_000)];
    assert_eq!(
        db.reset_budgets(BudgetEntity::Key, &resets).await.unwrap(),
        1
    );
    assert_eq!(
        db.reset_budgets(BudgetEntity::Key, &resets).await.unwrap(),
        0,
        "second run is a no-op"
    );
    let after = db.get_key("hash-a").await.unwrap().unwrap();
    assert_eq!((after.spend, after.budget_reset_at), (0.0, Some(90_000)));
    assert!(
        db.budgets_due(BudgetEntity::Key, 2_000, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn kv_upserts() {
    let (_dir, db) = fresh().await;
    assert_eq!(db.kv_get("x").await.unwrap(), None);
    db.kv_set("x", "1").await.unwrap();
    db.kv_set("x", "2").await.unwrap();
    assert_eq!(db.kv_get("x").await.unwrap().as_deref(), Some("2"));
    let _ = BTreeMap::<String, i64>::new();
}

#[tokio::test]
async fn rotation_keeps_the_alias() {
    let (_dir, db) = fresh().await;
    db.insert_key(&key("hash-x", Some("beta"))).await.unwrap();
    db.replace_key_token("hash-x", "hash-y", "sk-...sh-y")
        .await
        .unwrap();
    let rotated = db.get_key("hash-y").await.unwrap().unwrap();
    assert_eq!(rotated.key_alias.as_deref(), Some("beta"));
    assert_eq!(
        db.get_key_by_alias("beta").await.unwrap().unwrap().token,
        "hash-y"
    );
}
