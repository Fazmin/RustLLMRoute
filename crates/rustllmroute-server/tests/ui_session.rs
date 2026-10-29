use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::MockServer;

const MASTER: &str = "sk-master-test";

async fn boot(loaded: LoadedConfig) -> SocketAddr {
    let state = state::build(loaded, &Cli::parse_from(["rustllmroute"]))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app::router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

fn yaml(upstream: &str, extra_model: bool) -> String {
    let extra = if extra_model {
        format!(
            "  - model_name: second\n    litellm_params: {{ model: openai/gpt-4o, api_key: sk-second-secret, api_base: {upstream} }}\n    model_info: {{ id: dep-second }}\n"
        )
    } else {
        String::new()
    };
    format!(
        "model_list:\n  - model_name: gpt\n    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-live-secret, api_base: {upstream}, rpm: 10 }}\n    model_info: {{ id: dep-gpt, max_input_tokens: 1000 }}\n{extra}general_settings:\n  master_key: {MASTER}\n"
    )
}

#[tokio::test]
async fn login_sessions_reach_admin_routes_and_the_ui_is_served() {
    let upstream = MockServer::start().await;
    let loaded =
        LoadedConfig::parse(&yaml(&upstream.uri(), false), Path::new("test.yaml")).unwrap();
    let address = boot(loaded).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let wrong = client
        .post(format!("http://{address}/login"))
        .json(&json!({"username": "admin", "password": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let login = client
        .post(format!("http://{address}/login"))
        .json(&json!({"username": "admin", "password": MASTER}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let cookie = login.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("rustllmroute_ui="), "{cookie}");
    assert!(cookie.contains("HttpOnly"));
    let body: Value = login.json().await.unwrap();
    let token = body["token"].as_str().unwrap().to_owned();
    assert_eq!(body["user_role"], "proxy_admin");

    let cookie_pair = cookie.split(';').next().unwrap().to_owned();
    let info: Value = client
        .get(format!("http://{address}/model/info"))
        .header("cookie", &cookie_pair)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["data"][0]["model_info"]["id"], "dep-gpt");
    assert_eq!(info["data"][0]["litellm_params"]["api_key"], "***");
    assert_eq!(info["data"][0]["litellm_params"]["rpm"], 10);
    assert_eq!(info["data"][0]["model_info"]["max_input_tokens"], 1000);

    let groups: Value = client
        .get(format!("http://{address}/model_group/info"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(groups["data"][0]["model_group"], "gpt");
    assert_eq!(groups["data"][0]["deployments"], 1);
    assert_eq!(groups["data"][0]["providers"][0], "openai");

    let anonymous = client
        .get(format!("http://{address}/model/info"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let redirect = client
        .get(format!("http://{address}/ui"))
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), 308);
    assert_eq!(redirect.headers()["location"], "/ui/");
    let page = client
        .get(format!("http://{address}/ui/"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(
        page.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = page.text().await.unwrap();
    assert!(html.contains("RustLLMroute"), "{html}");
    let deep = client
        .get(format!("http://{address}/ui/keys"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        deep.status(),
        200,
        "client-side routes fall back to the app shell"
    );
    let missing = client
        .get(format!("http://{address}/ui/assets/nope.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    let logout = client
        .post(format!("http://{address}/logout"))
        .send()
        .await
        .unwrap();
    assert!(
        logout.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
}

#[tokio::test]
async fn config_yaml_is_redacted_and_reload_applies_the_model_list() {
    let upstream = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, yaml(&upstream.uri(), false)).unwrap();
    let loaded = LoadedConfig::load(&path).unwrap();
    let address = boot(loaded).await;
    let client = reqwest::Client::new();

    let view: Value = client
        .get(format!("http://{address}/config/yaml"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = view["yaml"].as_str().unwrap();
    assert!(!text.contains("sk-live-secret"), "{text}");
    assert!(!text.contains(MASTER), "{text}");
    assert!(text.contains("api_key: \"***\""), "{text}");
    assert!(text.contains("model: openai/gpt-4o-mini"), "{text}");
    assert_eq!(view["models"], 1);

    std::fs::write(&path, yaml(&upstream.uri(), true)).unwrap();
    let reload: Value = client
        .post(format!("http://{address}/config/reload"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reload["models"], 2, "{reload}");
    let models: Value = client
        .get(format!("http://{address}/v1/models"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"second"), "{ids:?}");

    std::fs::write(&path, "model_list: []\n").unwrap();
    let rejected = client
        .post(format!("http://{address}/config/reload"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap();
    assert_eq!(
        rejected.status(),
        400,
        "an invalid file keeps the old config live"
    );
    let still: Value = client
        .get(format!("http://{address}/v1/models"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(still["data"].as_array().unwrap().len(), 2);
}
