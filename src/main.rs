use axum::extract::{Json, Path};
use axum::routing::{get, post};
use once_cell::sync::OnceCell;

static DB: OnceCell<sqlx::Pool<sqlx::Postgres>> = OnceCell::new();

#[macro_use]
extern crate sqlx;

#[derive(serde::Deserialize)]
struct Config {
    db: String,
    port: u16,
}

#[tokio::main]
async fn main() {
    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("./config.toml"));

    let config_string = std::fs::read_to_string(&cfg_path).expect("Failed to read config");
    let config = toml::from_str::<Config>(&config_string).expect("Failed to parse config");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.db)
        .await
        .expect("Failed to connect to database!");
    migrate!("./migrations").run(&pool).await.unwrap();
    DB.set(pool).expect("Failed to set OnceCell");
    let app = axum::Router::new()
        .route("/", get(root))
        .route("/:path", get(getpaste))
        .route("/submit", post(submit));
    tokio::spawn(async move { delete_expired().await });
    axum::Server::bind(&std::net::SocketAddr::from(([127, 0, 0, 1], config.port)))
        .serve(app.into_make_service())
        .await
        .expect("Failed to bind to address, is something else using the port?");
}

axum_static_macro::static_file!(root, "index.html", axum_static_macro::content_types::HTML);

async fn submit(Json(req): Json<NewPaste>) -> Result<(axum::http::StatusCode, String), Error> {
    let persistence_length = chrono::Duration::weeks(1);
    let expires = chrono::offset::Local::now()
        .checked_add_signed(persistence_length)
        .ok_or(Error::TimeError)?;
    let key = random_string::generate(
        8,
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz1234567890-",
    );
    let db = DB.get().ok_or_else(|| Error::NoDb)?;
    query!(
        "INSERT INTO pastes VALUES ($1, $2, $3)",
        key,
        req.contents,
        expires
    )
    .execute(db)
    .await?;
    Ok((
        axum::http::StatusCode::OK,
        format!("{{\"message\": \"Paste submitted!\", \"id\": \"{}\"}}", key),
    ))
}
#[axum_macros::debug_handler]
async fn getpaste(
    Path(id): Path<String>,
) -> Result<(axum::http::StatusCode, axum::http::HeaderMap, String), Error> {
    let db = DB.get().ok_or_else(|| Error::NoDb)?;
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
                include_str!("404.html").to_string(),
            ));
        }
        Err(e) => return Err(Error::Sqlx(e)),
    };
    println!("{:#?}", res);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_static("text/html"),
    );
    Ok((axum::http::StatusCode::OK, headers, "".to_string()))
}

async fn delete_expired() {
    loop {
        let db = match DB.get() {
            Some(db) => db,
            None => continue,
        };
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

#[derive(serde::Deserialize, Debug, Clone)]
struct NewPaste {
    contents: String,
}

enum Error {
    // Errors
    TimeError,
    NoDb,
    Sqlx(sqlx::Error),

    // Expected errors
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let (body, status, content_type): (
            std::borrow::Cow<str>,
            axum::http::StatusCode,
            &'static str,
        ) = match self {
            Error::TimeError => (
                r#"{"error":"Bad request"}"#.into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "application/json",
            ),
            Error::NoDb => (
                r#"{"error":"Database connection failed"}"#.into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "application/json",
            ),
            Error::Sqlx(_) => (
                r#"{"error":"Database lookup failed"}"#.into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "application/json",
            ),
        };
        axum::response::Response::builder()
            .header(
                axum::http::header::CONTENT_TYPE,
                axum::http::header::HeaderValue::from_static(content_type),
            )
            .status(status)
            .body(axum::body::boxed(axum::body::Full::from(body)))
            .unwrap()
    }
}
