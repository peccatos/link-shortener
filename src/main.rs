use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Redirect,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::net::SocketAddr;
use std::str::FromStr;
use redis::aio::ConnectionManager;

type ApiError = (StatusCode, Json<Value>);

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.into() })))
}

fn not_found(msg: impl Into<String>) -> ApiError {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg.into() })))
}

fn internal_error(msg: impl Into<String>) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg.into() })),
    )
}

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    redis: ConnectionManager,
}

#[derive(Deserialize)]
struct ShortenRequest {
    url: String,
}

#[derive(Serialize)]
struct ShortenResponse {
    code: String,
    short_url: String,
    original_url: String,
}

#[derive(Serialize)]
struct StatsResponse {
    code: String,
    url: String,
    total_clicks: i64,
    last_click: Option<String>,
    top_user_agents: Vec<UserAgentStat>,
}

#[derive(Serialize, sqlx::FromRow)]
struct UserAgentStat {
    user_agent: String,
    clicks: i64,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "link_shortener" }))
}

async fn hello() -> Json<Value> {
    Json(json!({ "message": "привет" }))
}

async fn sum(Path((a, b)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let a = a.parse::<i64>().map_err(|_| bad_request(format!("'{a}' - не число")))?;
    let b = b.parse::<i64>().map_err(|_| bad_request(format!("'{b}' - не число")))?;
    Ok(Json(json!({ "a": a, "b": b, "result": a + b })))
}

async fn stats(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<StatsResponse>, ApiError> {
    // 1. Проверяем, что ссылка существует
    let link: Option<(String,)> = sqlx::query_as("SELECT url FROM links WHERE code = ?1")
        .bind(&code)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| internal_error(format!("БД: {e}")))?;

    let url = match link {
        Some((url,)) => url,
        None => return Err(not_found(format!("код '{code}' не найден"))),
    };

    // 2. Общая статистика (всегда возвращает одну строку, COUNT = 0 если пусто)
    let (total_clicks, last_click): (i64, Option<String>) = sqlx::query_as(
        "SELECT COUNT(*) AS total, MAX(clicked_at) AS last_click
         FROM clicks WHERE code = ?1",
    )
    .bind(&code)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| internal_error(format!("БД: {e}")))?;

    // 3. Топ User-Agent'ов
    let top_user_agents: Vec<UserAgentStat> = sqlx::query_as(
        "SELECT user_agent, COUNT(*) AS clicks
         FROM clicks WHERE code = ?1
         GROUP BY user_agent
         ORDER BY clicks DESC
         LIMIT 5",
    )
    .bind(&code)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| internal_error(format!("БД: {e}")))?;

    Ok(Json(StatsResponse {
        code,
        url,
        total_clicks,
        last_click,
        top_user_agents,
    }))
}

async fn shorten(
    State(state): State<AppState>,
    Json(req): Json<ShortenRequest>,
) -> Result<(StatusCode, Json<ShortenResponse>), ApiError> {
    let url = req.url.trim().to_string();
    if url.is_empty() {
        return Err(bad_request("url пустой"));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(bad_request("url должен начинаться с http:// или https://"));
    }

    // Пытаемся вставить. На случай коллизии по PRIMARY KEY (крайне маловероятно,
    // но теоретически возможно) — генерируем новый код и повторяем.
    let mut code = nanoid::nanoid!(7);
    loop {
        let result = sqlx::query("INSERT INTO links (code, url) VALUES (?1, ?2)")
            .bind(&code)
            .bind(&url)
            .execute(&state.pool)
            .await;

        match result {
            Ok(_) => break,
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                code = nanoid::nanoid!(7);
            }
            Err(e) => return Err(internal_error(format!("БД: {e}"))),
        }
    }

    let short_url = format!("http://localhost:3000/{code}");
    Ok((
        StatusCode::CREATED,
        Json(ShortenResponse { code, short_url, original_url: url }),
    ))
}

async fn redirect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Result<Redirect, ApiError> {
    // 1. Пробуем взять из Redis. Если Redis недоступен — просто идём в БД (fail-open).
    let mut conn = state.redis.clone();
    let cached: Option<String> = redis::cmd("GET")
        .arg(&code)
        .query_async(&mut conn)
        .await
        .ok()
        .flatten();

    let url = if let Some(url) = cached {
        println!("[CACHE HIT] {code} -> {url}");
        url
    } else {
        // 2. Cache miss — идём в SQLite
        println!("[CACHE MISS] {code}");
        let row: Option<(String,)> = sqlx::query_as("SELECT url FROM links WHERE code = ?1")
            .bind(&code)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| internal_error(format!("БД: {e}")))?;

        match row {
            Some((url,)) => {
                // 3. Кладём в Redis на 3600 секунд (1 час). Ошибку кэша игнорируем.
                let _: Result<(), _> = redis::cmd("SETEX")
                    .arg(&code)
                    .arg(3600)
                    .arg(&url)
                    .query_async(&mut conn)
                    .await;
                url
            }
            None => return Err(not_found(format!("код '{code}' не найден"))),
        }
    };

    // 4. Пишем клик в аналитику (как раньше)
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Unknown");
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");

    let _ = sqlx::query("INSERT INTO clicks (code, user_agent, ip) VALUES (?1, ?2, ?3)")
        .bind(&code)
        .bind(user_agent)
        .bind(ip)
        .execute(&state.pool)
        .await;

    Ok(Redirect::to(&url))
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Подключаемся к SQLite. create_if_missing — создать файл links.db,
    // если его ещё нет (в текущей папке, где запущен cargo run).
    let options = SqliteConnectOptions::from_str("sqlite://links.db")?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    let redis_client = redis::Client::open("redis://127.0.0.1/")?;
    let redis_conn = ConnectionManager::new(redis_client).await?;

    let state = AppState { pool, redis: redis_conn };

    let app = Router::new()
        .route("/", get(health))
        .route("/hello", get(hello))
        .route("/sum/{a}/{b}", get(sum))
        .route("/shorten", post(shorten))
        .route("/stats/{code}", get(stats))
        .route("/{code}", get(redirect))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Сервер запущен: http://localhost:3000");
    axum::serve(listener, app).await?;
    Ok(())
}

//sqlx::query(
//redirect
//ShortenResponse
//{code}
//pool 