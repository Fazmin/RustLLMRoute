use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

struct Gateway {
    address: SocketAddr,
    _dir: tempfile::TempDir,
    client: reqwest::Client,
}

impl Gateway {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn post(&self, path: &str, key: &str, body: Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str, key: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
    }

    async fn chat(&self, key: &str, model: &str) -> reqwest::Response {
        self.post(
            "/v1/chat/completions",
            key,
            json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
    }

    async fn generate_key(&self, body: Value) -> Value {
        let response = self.post("/key/generate", MASTER, body).await;
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        response.json().await.unwrap()
    }
}

async fn boot(upstream: &MockServer) -> Gateway {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir
        .path()
        .join("gateway.db")
        .display()
        .to_string()
        .replace('\\', "/");
    let yaml = format!(
        r#"
model_list:
  - model_name: gpt-4o-mini
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {base} }}
    model_info: {{ id: dep-a }}
  - model_name: other-model
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {base} }}
    model_info: {{ id: dep-b }}
general_settings:
  master_key: {MASTER}
  database_url: sqlite://{db_path}
  spend:
    flush_interval_s: 1
"#,
        base = upstream.uri()
    );
    let loaded = LoadedConfig::parse(&yaml, Path::new("test.yaml")).unwrap();
    let state = state::build(loaded, &Cli::parse_from(["rustllmroute"]))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app::router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Gateway {
        address,
        _dir: dir,
        client: reqwest::Client::new(),
    }
}

fn completion() -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 500, "total_tokens": 1500}
    })
}

async fn mock_ok(upstream: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion()))
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn virtual_key_lifecycle_and_spend_tracking() {
    let upstream = MockServer::start().await;
    mock_ok(&upstream).await;
    let gateway = boot(&upstream).await;

    let created = gateway
        .generate_key(json!({"key_alias": "team-a-key", "models": ["gpt-4o-mini"], "max_budget": 1.0, "metadata": {"tags": ["t1"]}}))
        .await;
    let raw = created["key"].as_str().unwrap().to_owned();
    assert!(raw.starts_with("sk-"));
    let hash = created["token"].as_str().unwrap().to_owned();
    assert_eq!(hash.len(), 64);
    assert_eq!(created["models"], json!(["gpt-4o-mini"]));

    let response = gateway.chat(&raw, "gpt-4o-mini").await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let cost: f64 = response.headers()["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0);

    let denied = gateway.chat(&raw, "other-model").await;
    assert_eq!(denied.status(), 403);
    let body: Value = denied.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");

    assert_eq!(
        gateway
            .chat("sk-does-not-exist", "gpt-4o-mini")
            .await
            .status(),
        401
    );
    assert_eq!(
        gateway.get("/key/generate", &raw).await.status(),
        405,
        "GET on a POST route"
    );
    let forbidden = gateway.post("/key/generate", &raw, json!({})).await;
    assert_eq!(forbidden.status(), 403, "non-admin keys cannot mint keys");

    let mut logs = Vec::new();
    for _ in 0..40 {
        let response = gateway
            .get(&format!("/spend/logs?api_key={hash}"), MASTER)
            .await;
        assert_eq!(response.status(), 200);
        logs = response.json::<Vec<Value>>().await.unwrap();
        if logs.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(logs.len(), 2, "one success and one failure row");
    let success = logs.iter().find(|l| l["status"] == "success").unwrap();
    assert_eq!(success["prompt_tokens"], 1000);
    assert!(success["spend"].as_f64().unwrap() > 0.0);
    assert_eq!(success["model_id"], "dep-a");
    let failure = logs.iter().find(|l| l["status"] == "failure").unwrap();
    assert_eq!(failure["error"]["status_code"], 403);

    let info: Value = gateway
        .get(&format!("/key/info?key={raw}"), MASTER)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        info["info"]["spend"].as_f64().unwrap() > 0.0,
        "key spend counter was flushed"
    );
    assert!(
        info["info"].get("key").is_none(),
        "raw key is never echoed back"
    );
    let own: Value = gateway.get("/key/info", &raw).await.json().await.unwrap();
    assert_eq!(own["key"], hash);

    let global: Value = gateway
        .get("/global/spend", MASTER)
        .await
        .json()
        .await
        .unwrap();
    assert!(global["spend"].as_f64().unwrap() > 0.0);
    let activity: Value = gateway
        .get("/user/daily/activity", MASTER)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(activity["metadata"]["total_api_requests"], 2);
    assert_eq!(
        activity["results"][0]["breakdown"]["models"]["gpt-4o-mini"]["metrics"]["successful_requests"],
        1
    );

    let blocked = gateway
        .post("/key/block", MASTER, json!({"key": raw}))
        .await;
    assert_eq!(blocked.status(), 200);
    assert_eq!(gateway.chat(&raw, "gpt-4o-mini").await.status(), 403);
    gateway
        .post("/key/unblock", MASTER, json!({"key": raw}))
        .await;
    assert_eq!(gateway.chat(&raw, "gpt-4o-mini").await.status(), 200);

    let regenerated: Value = gateway
        .post(&format!("/key/{raw}/regenerate"), MASTER, json!({}))
        .await
        .json()
        .await
        .unwrap();
    let new_raw = regenerated["key"].as_str().unwrap().to_owned();
    assert_ne!(new_raw, raw);
    assert_eq!(
        gateway.chat(&raw, "gpt-4o-mini").await.status(),
        401,
        "old key is gone"
    );
    assert_eq!(gateway.chat(&new_raw, "gpt-4o-mini").await.status(), 200);

    let deleted: Value = gateway
        .post("/key/delete", MASTER, json!({"keys": [new_raw]}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["deleted_keys"].as_array().unwrap().len(), 1);
    assert_eq!(gateway.chat(&new_raw, "gpt-4o-mini").await.status(), 401);
}

#[tokio::test]
async fn budgets_and_rate_limits_are_enforced() {
    let upstream = MockServer::start().await;
    mock_ok(&upstream).await;
    let gateway = boot(&upstream).await;

    let tiny = gateway.generate_key(json!({"max_budget": 0.0000001})).await;
    let tiny_key = tiny["key"].as_str().unwrap();
    assert_eq!(
        gateway.chat(tiny_key, "gpt-4o-mini").await.status(),
        200,
        "first call fits"
    );
    let over = gateway.chat(tiny_key, "gpt-4o-mini").await;
    assert_eq!(over.status(), 400);
    let body: Value = over.json().await.unwrap();
    assert_eq!(body["error"]["code"], "budget_exceeded");

    let limited = gateway.generate_key(json!({"rpm_limit": 1})).await;
    let limited_key = limited["key"].as_str().unwrap();
    let first = gateway.chat(limited_key, "gpt-4o-mini").await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-ratelimit-limit-requests"], "1");
    assert_eq!(first.headers()["x-ratelimit-remaining-requests"], "0");
    let second = gateway.chat(limited_key, "gpt-4o-mini").await;
    assert_eq!(second.status(), 429);
    assert!(second.headers().contains_key("retry-after"));
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");

    let team: Value = gateway
        .post(
            "/team/new",
            MASTER,
            json!({"team_alias": "research", "models": ["gpt-4o-mini"], "max_budget": 0.0000001}),
        )
        .await
        .json()
        .await
        .unwrap();
    let team_id = team["team_id"].as_str().unwrap();
    let member = gateway.generate_key(json!({"team_id": team_id})).await;
    let member_key = member["key"].as_str().unwrap();
    assert_eq!(gateway.chat(member_key, "gpt-4o-mini").await.status(), 200);
    assert_eq!(
        gateway.chat(member_key, "gpt-4o-mini").await.status(),
        400,
        "team budget is shared"
    );
    assert_eq!(
        gateway.chat(member_key, "other-model").await.status(),
        403,
        "team model list bounds its keys"
    );

    let readiness: Value = gateway
        .get("/health/readiness", "")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(readiness["db"], "connected");
}
