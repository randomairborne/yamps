// TODO RATELIMITS

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, TypedHeader},
    headers::ContentLength,
    routing::get,
};

#[macro_use]
extern crate sqlx;
#[macro_use]
extern crate tracing;

#[derive(serde::Deserialize, Clone, Debug)]
struct Config {
    db: String,
    port: u16,
    dmca_email: String,
    size_limit: Option<u64>,
    cache: Option<usize>,
}

#[derive(Clone, Debug)]
struct State {
    config: Config,
    db: sqlx::PgPool
}

struct Cache {
    data: dashmap::DashMap<String, String>,
    expiries: parking_lot::RwLock<
        std::collections::BinaryHeap<(chrono::DateTime<chrono::Local>, String)>,
    >,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("LOG"))
        .init();
    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("./config.toml"));

    let config_string = std::fs::read_to_string(&cfg_path).expect("Failed to read config");
    let config = toml::from_str::<Config>(&config_string).expect("Failed to parse config");

    let mut tera = tera::Tera::default();
    tera.add_raw_template("paste.html", include_str!("./paste.html"))
        .expect("Failed to load paste.html as template");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.db)
        .await
        .expect("Failed to connect to database!");
    migrate!("./migrations").run(&pool).await.unwrap();
    let state = State {
        config: config.clone(),
        db: pool,
    };
    let cache: Arc<Cache> = Arc::new(Cache {
        data: dashmap::DashMap::new(),
        expiries: parking_lot::RwLock::new(std::collections::BinaryHeap::new()),
    });
    let add_state = state.clone();
    let view_state = state.clone();
    let deleter_state = state.clone();
    let add_cache = cache.clone();
    let view_cache = cache.clone();
    let app = axum::Router::new()
        .route(
            "/",
            get(root).post(move |typedheader, multipart| {
                submit(typedheader, multipart, add_state, add_cache)
            }),
        )
        .route(
            "/:path",
            get(move |id| getpaste(id, view_state, view_cache, tera)),
        );
    tokio::spawn(async move { delete_expired(&deleter_state.db).await });
    tokio::spawn(async move { clear_cache(cache, config.cache).await });
    warn!("Listening on http://0.0.0.0:{} (http)", config.port);
    axum::Server::bind(&std::net::SocketAddr::from(([0, 0, 0, 0], config.port)))
        .serve(app.into_make_service())
        .await
        .expect("Failed to bind to address, is something else using the port?");
}

axum_static_macro::static_file!(root, "index.html", axum_static_macro::content_types::HTML);

async fn submit(
    TypedHeader(length): TypedHeader<ContentLength>,
    mut multipart: Multipart,
    state: State,
    cache: Arc<Cache>,
) -> Result<(axum::http::StatusCode, axum::http::HeaderMap, String), Error> {
    if length.0 > state.config.size_limit.unwrap_or(1024) * 1024 {
        return Ok((
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            axum::http::HeaderMap::new(),
            "Paste too long!".to_string(),
        ));
    }
    let mut data = String::new();
    while let Some(field) = multipart.next_field().await? {
        if field.name().ok_or(Error::FieldInvalid)? == "contents" {
            data = field.text().await?;
            break;
        }
    }

    let persistence_length = chrono::Duration::weeks(1);
    let expires = chrono::offset::Local::now()
        .checked_add_signed(persistence_length)
        .ok_or(Error::TimeError)?;
    let key = random_string::generate(
        8,
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz1234567890",
    );
    let db = &state.db;
    let contents = tera::escape_html(&data);
    query!(
        "INSERT INTO pastes VALUES ($1, $2, $3)",
        key,
        &contents,
        expires
    )
    .execute(db)
    .await?;
    if let Some(_) = state.config.cache {
        let mut heap = cache.expiries.write();

        cache.data.insert(key.clone(), contents);
        heap.push((chrono::offset::Local::now(), key.clone()));
    }

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::LOCATION,
        axum::http::header::HeaderValue::from_str(&format!("/{}", key))?,
    );
    Ok((
        axum::http::StatusCode::FOUND,
        headers,
        "Paste submitted!".to_string(),
    ))
}

async fn getpaste(
    Path(id): Path<String>,
    state: State,
    cache: Arc<Cache>,
    tera: tera::Tera,
) -> Result<(axum::http::StatusCode, axum::http::HeaderMap, String), Error> {
    let contents: String;
    // TODO replace this with let chaining when rust 1.62 is released
    if let (Some(_), Some(item)) = (state.config.cache, cache.data.get(&id)) {
        contents = item.value().to_string();
        trace!("Cache hit!");
    } else {
        let db = &state.db;
        let res = match query!("SELECT contents FROM pastes WHERE key = $1", id)
            .fetch_one(db)
            .await
        {
            Ok(data) => data,
            Err(sqlx::Error::RowNotFound) => {
                let mut headers = axum::http::HeaderMap::new();
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::HeaderValue::from_static("text/html"),
                );
                return Ok((
                    axum::http::StatusCode::NOT_FOUND,
                    headers,
                    include_str!("./404.html").to_string(),
                ));
            }
            Err(e) => return Err(Error::Sqlx(e)),
        };
        contents = res.contents.ok_or(Error::InternalError)?;
    };
    let mut context = tera::Context::new();
    context.insert("dmca_email", &state.config.dmca_email);
    context.insert("paste_contents", &contents);
    let final_contents = tera.render("paste.html", &context)?;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_static("text/html"),
    );
    Ok((axum::http::StatusCode::OK, headers, final_contents))
}

async fn delete_expired(db: &sqlx::PgPool) {
    loop {
        info!("Deleting old pastes...");
        let now: chrono::DateTime<chrono::Local> = chrono::Local::now();
        match query!("DELETE FROM pastes WHERE expires < $1", now)
            .execute(db)
            .await
        {
            Ok(_) => {}
            Err(e) => tracing::error!("Error deleting expired pastes: {}", e),
        };
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

// This was O(n^n), thanks to tazz4843 for fixing that
async fn clear_cache(cache: Arc<Cache>, max: Option<usize>) {
    if let Some(max_size) = max {
        let max_size = max_size * 1_048_576;
        loop {
            debug!("Clearing cache...");
            let mut size: usize = 0;
            for item in cache.data.iter() {
                size += item.value().capacity();
            }
            while size > max_size {
                let heap = cache.expiries.upgradable_read();
                if let Some(item) = heap.peek() {
                    size -= item.1.capacity();
                    cache.data.remove(&item.1);
                    let mut rwheap = parking_lot::RwLockUpgradableReadGuard::upgrade(heap);
                    rwheap.pop();
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
}

#[derive(Debug)]
enum Error {
    // Errors
    TimeError,
    FieldInvalid,
    InternalError,
    InvalidHeaderValue(axum::http::header::InvalidHeaderValue),
    Sqlx(sqlx::Error),
    Multipart(axum::extract::multipart::MultipartError),
    TemplatingError(tera::Error),
}

impl From<axum::http::header::InvalidHeaderValue> for Error {
    fn from(e: axum::http::header::InvalidHeaderValue) -> Self {
        Self::InvalidHeaderValue(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl From<axum::extract::multipart::MultipartError> for Error {
    fn from(e: axum::extract::multipart::MultipartError) -> Self {
        Self::Multipart(e)
    }
}

impl From<tera::Error> for Error {
    fn from(e: tera::Error) -> Self {
        Self::TemplatingError(e)
    }
}

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let (body, status): (std::borrow::Cow<str>, axum::http::StatusCode) = match self {
            Error::TimeError => ("Bad request".into(), axum::http::StatusCode::BAD_REQUEST),
            Error::FieldInvalid => (
                "HTTP field invalid".into(),
                axum::http::StatusCode::BAD_REQUEST,
            ),
            Error::Multipart(_) => (
                "MultiPartFormData invalid".into(),
                axum::http::StatusCode::BAD_REQUEST,
            ),
            Error::InternalError => (
                "Unknown internal error".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::InvalidHeaderValue(_) => (
                "Invalid redirect value (this should be impossible)".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::Sqlx(_) => (
                "Database lookup failed".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),

            Error::TemplatingError(_) => (
                "Templating library error".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
        };
        if status == axum::http::StatusCode::INTERNAL_SERVER_ERROR {
            error!("{:#?}", self);
        } else {
            warn!("{:?}", self);
        }
        axum::response::Response::builder()
            .status(status)
            .body(axum::body::boxed(axum::body::Full::from(body)))
            .unwrap()
    }
}
