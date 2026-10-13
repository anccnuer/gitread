use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

/// 共享状态：一个复用的 reqwest 客户端（连接池 + 默认请求头）
struct AppState {
    client: reqwest::Client,
}

const INDEX_HTML: &str = include_str!("../index.html");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Wasmer Edge 通过 PORT 环境变量注入监听端口
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    // 可选：配置 GITHUB_TOKEN 提升 API 速率限制（60/h -> 5000/h）
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("gitread-edge/0.1 (+wasmer)"),
    );
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            if let Ok(v) =
                HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            {
                headers.insert(header::AUTHORIZATION, v);
            }
        }
    }

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let state = Arc::new(AppState { client });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/readme", get(readme))
        .route("/api/releases", get(releases))
        .route("/api/download", get(download))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    println!("gitread-edge listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
}

/// 从各种 GitHub 链接形式解析出 (owner, repo)。
/// 支持：
///   https://github.com/owner/repo
///   https://github.com/owner/repo/tree/main/src/...
///   git@github.com:owner/repo.git
///   owner/repo
fn parse_owner_repo(raw: &str) -> Result<(String, String), (StatusCode, String)> {
    let s = raw.trim();
    if s.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "链接为空".into()));
    }

    // 去掉协议前缀
    let mut body = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .or_else(|| s.strip_prefix("git://"))
        .unwrap_or(s);

    // 处理 SSH 形式：git@github.com:owner/repo(.git)
    if let Some(at) = body.find('@') {
        body = &body[at + 1..];
    }

    // 去掉主机名部分
    // 冒号分隔（ssh）或斜杠分隔（https）
    let path = if let Some(colon) = body.find(':') {
        // 形如 github.com:owner/repo 或 github.com/owner/repo（无端口时）
        let after = &body[colon + 1..];
        if after.contains('/') {
            after
        } else if let Some(slash) = body.find('/') {
            // host/owner/repo 情况
            &body[slash + 1..]
        } else {
            after
        }
    } else if let Some(slash) = body.find('/') {
        &body[slash + 1..]
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            "无法识别的 GitHub 链接".into(),
        ));
    };

    let mut segs = path
        .trim_end_matches('/')
        .split('/')
        .filter(|x| !x.is_empty());
    let owner = segs.next().unwrap_or_default().trim_end_matches(".git");
    let repo = segs.next().unwrap_or_default().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "需要 owner/repo，例如 https://github.com/tokio-rs/tokio".into(),
        ));
    }
    Ok((owner.to_string(), repo.to_string()))
}

fn bad_gateway(err: reqwest::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": format!("请求 GitHub 失败: {err}") })),
    )
}

/// raw.githubusercontent 上的常见 README 文件名（大小写敏感，按命中率排序）
const README_CANDIDATES: &[&str] = &[
    "README.md",
    "readme.md",
    "README",
    "Readme.md",
    "README.MD",
    "README.txt",
    "readme.txt",
];

/// GET /api/readme?url=<github link>
/// 优先直接拼 raw.githubusercontent.com（CDN、不占 API 限额），
/// HEAD 自动解析默认分支；全部落空再回退 API /readme（可找到非根目录/罕见命名的 README）。
async fn readme(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UrlQuery>,
) -> Response {
    let (owner, repo) = match parse_owner_repo(&q.url) {
        Ok(v) => v,
        Err((code, msg)) => {
            return (code, Json(json!({ "error": msg }))).into_response()
        }
    };

    for name in README_CANDIDATES {
        let raw_url =
            format!("https://raw.githubusercontent.com/{owner}/{repo}/HEAD/{name}");
        let resp = match state
            .client
            .get(&raw_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        if resp.status().is_success() {
            if let Ok(content) = resp.text().await {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "owner": owner,
                        "repo": repo,
                        "file": name,
                        "content": content,
                    })),
                )
                    .into_response();
            }
        }
    }

    let api = format!("https://api.github.com/repos/{owner}/{repo}/readme");
    let resp = match state
        .client
        .get(&api)
        .header(header::ACCEPT, "application/vnd.github.raw+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return bad_gateway(e).into_response(),
    };

    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{owner}/{repo} 未找到 README 或仓库不存在") })),
        )
            .into_response();
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return (
            status,
            Json(json!({ "error": format!("GitHub 返回 {status}: {text}") })),
        )
            .into_response();
    }

    match resp.text().await {
        Ok(content) => (
            StatusCode::OK,
            Json(json!({ "owner": owner, "repo": repo, "content": content })),
        )
            .into_response(),
        Err(e) => bad_gateway(e).into_response(),
    }
}

/// GET /api/releases?url=<github link>
async fn releases(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UrlQuery>,
) -> Response {
    let (owner, repo) = match parse_owner_repo(&q.url) {
        Ok(v) => v,
        Err((code, msg)) => {
            return (code, Json(json!({ "error": msg }))).into_response()
        }
    };

    let api = format!(
        "https://api.github.com/repos/{owner}/{repo}/releases?per_page=20"
    );
    let resp = match state
        .client
        .get(&api)
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return bad_gateway(e).into_response(),
    };

    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{owner}/{repo} 仓库不存在") })),
        )
            .into_response();
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return (
            status,
            Json(json!({ "error": format!("GitHub 返回 {status}: {text}") })),
        )
            .into_response();
    }

    match resp.json::<serde_json::Value>().await {
        Ok(list) => {
            let out = list.as_array().map(|arr| {
                arr.iter()
                    .map(|r| {
                        let assets = r
                            .get("assets")
                            .and_then(|a| a.as_array())
                            .map(|a| {
                                a.iter()
                                    .map(|asset| {
                                        json!({
                                            "name": asset.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                            "size": asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                                            "browser_download_url": asset.get("browser_download_url").and_then(|u| u.as_str()).unwrap_or(""),
                                        })
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        json!({
                            "tag_name": r.get("tag_name").and_then(|t| t.as_str()).unwrap_or(""),
                            "name": r.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "published_at": r.get("published_at").and_then(|p| p.as_str()).unwrap_or(""),
                            "prerelease": r.get("prerelease").and_then(|p| p.as_bool()).unwrap_or(false),
                            "assets": assets,
                        })
                    })
                    .collect::<Vec<_>>()
            });
            (
                StatusCode::OK,
                Json(json!({ "owner": owner, "repo": repo, "releases": out.unwrap_or_default() })),
            )
                .into_response()
        }
        Err(e) => bad_gateway(e).into_response(),
    }
}

#[derive(Deserialize)]
struct DownloadQuery {
    /// 目标资源的 browser_download_url
    url: String,
    /// 下载保存的文件名（可选）
    name: Option<String>,
}

/// 只允许 GitHub 官方下载域名，防止被当作开放代理（SSRF）。
fn allowed_asset_host(url: &str) -> bool {
    let m = match reqwest::Url::parse(url) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if m.scheme() != "https" {
        return false;
    }
    matches!(
        m.host_str(),
        Some(
            "github.com"
                | "objects.githubusercontent.com"
                | "codeload.github.com"
                | "raw.githubusercontent.com"
                | "release-assets.githubusercontent.com"
        )
    )
}

fn sanitize_filename(name: &str) -> String {
    name.rsplit('/').next().unwrap_or("download").to_string()
}

/// GET /api/download?url=<asset url>&name=<file>
/// 经 Wasmer Edge 出站下载并流式转发给浏览器，起到加速/代理作用。
async fn download(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    if !allowed_asset_host(&q.url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅允许 GitHub 官方资源域名" })),
        )
            .into_response();
    }

    let resp = match state.client.get(&q.url).send().await {
        Ok(r) => r,
        Err(e) => return bad_gateway(e).into_response(),
    };

    let status = resp.status();
    let upstream_ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));

    let filename = q
        .name
        .filter(|n| !n.trim().is_empty())
        .map(|n| sanitize_filename(&n))
        .unwrap_or_else(|| {
            let path = reqwest::Url::parse(&q.url)
                .map(|u| u.path().to_string())
                .unwrap_or_default();
            sanitize_filename(&path)
        });

    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);

    let cd = format!("attachment; filename=\"{filename}\"");
    let mut out = Response::builder().status(status.as_u16());
    out = out.header(header::CONTENT_TYPE, upstream_ct);
    if let Ok(cdval) = HeaderValue::from_str(&cd) {
        out = out.header(header::CONTENT_DISPOSITION, cdval);
    }
    out.body(body).unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "构造响应失败" })),
        )
            .into_response()
    })
}
