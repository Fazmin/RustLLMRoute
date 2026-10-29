//! The admin dashboard: a Vite build embedded at compile time and served under `/ui/`.

use axum::body::Body;
use axum::extract::Path;
use axum::response::{IntoResponse, Redirect, Response};
use http::{HeaderValue, StatusCode, header};

#[derive(rust_embed::Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../ui/dist/"]
#[allow_missing = true]
struct UiAssets;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>RustLLMroute</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color:#1f2937}code{background:#f3f4f6;padding:.1rem .3rem;border-radius:.25rem}</style></head>
<body><h1>RustLLMroute</h1>
<p>The admin dashboard was not built into this binary.</p>
<p>Build it with <code>npm --prefix ui install &amp;&amp; npm --prefix ui run build</code> before <code>cargo build</code>, or use the Docker image, which includes it.</p>
<p>The API is up: try <a href="/health/readiness">/health/readiness</a>.</p>
</body></html>"#;

pub async fn redirect_to_ui() -> Redirect {
    Redirect::permanent("/ui/")
}

pub async fn index() -> Response {
    serve("index.html")
}

pub async fn asset(Path(path): Path<String>) -> Response {
    serve(&path)
}

fn file_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = file.metadata.mimetype().to_owned();
    let immutable = path.starts_with("assets/");
    let mut response = Response::new(Body::from(file.data.into_owned()));
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    response
}

fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let candidate = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = UiAssets::get(candidate) {
        return file_response(candidate, file);
    }
    let wants_page = candidate == "index.html"
        || !candidate
            .rsplit('/')
            .next()
            .unwrap_or(candidate)
            .contains('.');
    if wants_page {
        if let Some(index) = UiAssets::get("index.html") {
            return file_response("index.html", index);
        }
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            PLACEHOLDER,
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}
