use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustllmroute_catalog::{Mode, ModelPricing};
use rustllmroute_router::{
    Attempt, AttemptUsage, Endpoint, Registry, RequestCtx, RoutedDeployment, Router, RouterConfig,
    RouterMeta, current_minute,
};
use rustllmroute_types::{ErrorKind, GatewayError};

struct Dep {
    id: Arc<str>,
    group: Arc<str>,
    meta: RouterMeta,
    pricing: ModelPricing,
}

impl RoutedDeployment for Dep {
    fn id(&self) -> &Arc<str> {
        &self.id
    }
    fn model_group(&self) -> &Arc<str> {
        &self.group
    }
    fn meta(&self) -> &RouterMeta {
        &self.meta
    }
    fn mode(&self) -> Mode {
        Mode::Chat
    }
    fn pricing(&self) -> &ModelPricing {
        &self.pricing
    }
    fn provider_model_for(&self, requested: &str) -> String {
        format!("{}:{requested}", self.id)
    }
}

fn dep(id: &str, group: &str, order: Option<i32>) -> Arc<Dep> {
    Arc::new(Dep {
        id: Arc::from(id),
        group: Arc::from(group),
        meta: RouterMeta {
            order,
            ..RouterMeta::default()
        },
        pricing: ModelPricing::default(),
    })
}

fn upstream(status: u16, kind: ErrorKind) -> GatewayError {
    GatewayError::Upstream {
        provider: "test",
        status,
        kind,
        code: None,
        message: format!("upstream {status}"),
        retry_after: None,
        body: None,
    }
}

type Script = Arc<Mutex<HashMap<String, VecDeque<GatewayError>>>>;

fn script(entries: &[(&str, Vec<GatewayError>)]) -> Script {
    Arc::new(Mutex::new(
        entries
            .iter()
            .map(|(id, errors)| ((*id).to_owned(), errors.iter().cloned().collect()))
            .collect(),
    ))
}

fn fallbacks(from: &str, to: &[&str]) -> Vec<std::collections::BTreeMap<String, Vec<String>>> {
    vec![
        [(
            from.to_owned(),
            to.iter().map(|s| (*s).to_owned()).collect(),
        )]
        .into_iter()
        .collect(),
    ]
}

/// Runs one request; each attempt pops the next scripted error for its deployment (none = success).
async fn run(
    router: &Router<Dep>,
    script: &Script,
    model: &str,
) -> (Result<(String, u32, u32), GatewayError>, Vec<String>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let aliases = HashMap::new();
    let ctx = RequestCtx::new(Endpoint::ChatCompletions, false, &aliases);
    let result = router
        .execute(model, &ctx, |selected| {
            let script = script.clone();
            let log = log.clone();
            async move {
                log.lock().unwrap().push(selected.model.clone());
                let next = script
                    .lock()
                    .unwrap()
                    .get_mut(&*selected.deployment.id)
                    .and_then(|queue| queue.pop_front());
                match next {
                    Some(error) => Err(error),
                    None => Ok(Attempt {
                        value: selected.deployment.id.to_string(),
                        usage: Some(AttemptUsage {
                            prompt_tokens: 10,
                            completion_tokens: 5,
                        }),
                        ttft: None,
                    }),
                }
            }
        })
        .await
        .map(|outcome| {
            (
                outcome.value,
                outcome.attempted_retries,
                outcome.attempted_fallbacks,
            )
        });
    let log = log.lock().unwrap().clone();
    (result, log)
}

fn ids(log: &[String]) -> Vec<&str> {
    log.iter().map(|m| m.split(':').next().unwrap()).collect()
}

#[tokio::test(start_paused = true)]
async fn rate_limit_moves_to_sibling_and_cools_the_failed_deployment() {
    let router = Router::new(
        Registry::new(
            vec![dep("a", "g", Some(0)), dep("b", "g", Some(1))],
            HashMap::new(),
        ),
        RouterConfig::default(),
    );
    let script = script(&[("a", vec![upstream(429, ErrorKind::RateLimit)])]);
    let (result, log) = run(&router, &script, "g").await;
    let (value, retries, fallbacks) = result.unwrap();
    assert_eq!(value, "b");
    assert_eq!(ids(&log), vec!["a", "b"]);
    assert_eq!((retries, fallbacks), (1, 0));
    let cooled: Vec<String> = router
        .store()
        .active_cooldowns()
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();
    assert_eq!(cooled, vec!["a"]);
    assert_eq!(
        router
            .stats_for(&Arc::from("b"))
            .minute(current_minute())
            .tpm,
        15
    );
    let (_, log) = run(&router, &script, "g").await;
    assert_eq!(ids(&log), vec!["b"], "a stays out of rotation while cooled");
}

#[tokio::test(start_paused = true)]
async fn server_errors_retry_with_backoff_then_fall_back() {
    let cfg = RouterConfig {
        fallbacks: fallbacks("g", &["h"]),
        ..RouterConfig::default()
    };
    let router = Router::new(
        Registry::new(
            vec![dep("a", "g", None), dep("c", "h", None)],
            HashMap::new(),
        ),
        cfg,
    );
    let errors = vec![upstream(503, ErrorKind::Upstream5xx); 3];
    let script = script(&[("a", errors)]);
    let started = tokio::time::Instant::now();
    let (result, log) = run(&router, &script, "g").await;
    let (value, retries, fallbacks) = result.unwrap();
    assert_eq!(value, "c");
    assert_eq!(ids(&log), vec!["a", "a", "a", "c"]);
    assert_eq!((retries, fallbacks), (2, 1));
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "0.5s + 1s backoff without siblings"
    );
}

#[tokio::test(start_paused = true)]
async fn all_cooled_down_yields_retry_after_until_the_cooldown_expires() {
    let cfg = RouterConfig {
        cooldown_time: Duration::from_secs(10),
        ..RouterConfig::default()
    };
    let router = Router::new(
        Registry::new(
            vec![dep("a", "g", Some(0)), dep("b", "g", Some(1))],
            HashMap::new(),
        ),
        cfg,
    );
    let script = script(&[
        ("a", vec![upstream(429, ErrorKind::RateLimit)]),
        ("b", vec![upstream(429, ErrorKind::RateLimit)]),
    ]);
    let (result, log) = run(&router, &script, "g").await;
    assert_eq!(ids(&log), vec!["a", "b"]);
    assert!(matches!(
        result,
        Err(GatewayError::Upstream { status: 429, .. })
    ));
    let (result, log) = run(&router, &script, "g").await;
    assert!(
        log.is_empty(),
        "nothing is attempted while everything is cooled"
    );
    match result {
        Err(GatewayError::NoDeployments {
            retry_after: Some(wait),
            ..
        }) => {
            assert!(wait <= Duration::from_secs(10) && wait >= Duration::from_secs(1));
        }
        other => panic!("expected NoDeployments with retry_after, got {other:?}"),
    }
    tokio::time::advance(Duration::from_secs(11)).await;
    let (result, _) = run(&router, &script, "g").await;
    assert!(result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn auth_errors_skip_the_deployment_and_cool_it_immediately() {
    let router = Router::new(
        Registry::new(
            vec![dep("a", "g", Some(0)), dep("b", "g", Some(1))],
            HashMap::new(),
        ),
        RouterConfig::default(),
    );
    let script = script(&[("a", vec![upstream(401, ErrorKind::Auth)])]);
    let (result, log) = run(&router, &script, "g").await;
    assert_eq!(result.unwrap().0, "b");
    assert_eq!(ids(&log), vec!["a", "b"]);
    assert_eq!(router.store().active_cooldowns().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn bad_requests_never_retry_or_fall_back() {
    let cfg = RouterConfig {
        fallbacks: fallbacks("g", &["h"]),
        ..RouterConfig::default()
    };
    let router = Router::new(
        Registry::new(
            vec![dep("a", "g", None), dep("c", "h", None)],
            HashMap::new(),
        ),
        cfg,
    );
    let script = script(&[("a", vec![upstream(400, ErrorKind::BadRequest)])]);
    let (result, log) = run(&router, &script, "g").await;
    assert!(matches!(
        result,
        Err(GatewayError::Upstream { status: 400, .. })
    ));
    assert_eq!(ids(&log), vec!["a"]);
}

#[tokio::test(start_paused = true)]
async fn context_window_errors_use_their_dedicated_fallbacks() {
    let cfg = RouterConfig {
        context_window_fallbacks: fallbacks("g", &["big"]),
        fallbacks: fallbacks("g", &["h"]),
        ..RouterConfig::default()
    };
    let router = Router::new(
        Registry::new(
            vec![
                dep("a", "g", None),
                dep("c", "h", None),
                dep("x", "big", None),
            ],
            HashMap::new(),
        ),
        cfg,
    );
    let script = script(&[("a", vec![upstream(400, ErrorKind::ContextWindow)])]);
    let (result, log) = run(&router, &script, "g").await;
    assert_eq!(result.unwrap().0, "x");
    assert_eq!(ids(&log), vec!["a", "x"]);
}

#[tokio::test(start_paused = true)]
async fn client_fallbacks_and_wildcards_resolve() {
    let router = Router::new(
        Registry::new(
            vec![dep("a", "g", None), dep("w", "wild/*", None)],
            HashMap::new(),
        ),
        RouterConfig::default(),
    );
    assert!(matches!(
        run(&router, &script(&[]), "nope").await.0,
        Err(GatewayError::NotFound(_))
    ));
    let (_, log) = run(&router, &script(&[]), "wild/foo").await;
    assert_eq!(log, vec!["w:wild/foo"]);
    let script = script(&[("a", vec![upstream(503, ErrorKind::Upstream5xx); 3])]);
    let aliases = HashMap::new();
    let mut ctx = RequestCtx::new(Endpoint::ChatCompletions, false, &aliases);
    let body = vec!["wild/x".to_owned()];
    ctx.body_fallbacks = Some(&body);
    ctx.num_retries = Some(0);
    let outcome = router
        .execute("g", &ctx, |selected| {
            let script = script.clone();
            async move {
                let next = script
                    .lock()
                    .unwrap()
                    .get_mut(&*selected.deployment.id)
                    .and_then(|q| q.pop_front());
                match next {
                    Some(error) => Err(error),
                    None => Ok(Attempt {
                        value: selected.model.clone(),
                        usage: None,
                        ttft: None,
                    }),
                }
            }
        })
        .await
        .unwrap();
    assert_eq!(outcome.value, "w:wild/x");
    assert_eq!(
        (outcome.attempted_retries, outcome.attempted_fallbacks),
        (0, 1)
    );
}
