use axum::extract::{ContentLengthLimit, Multipart, Path};
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
   tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("LOG"))
        .init();
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
    axum::Server::bind(&std::net::SocketAddr::from(([0, 0, 0, 0], config.port)))
        .serve(app.into_make_service())
        .await
        .expect("Failed to bind to address, is something else using the port?");
}

axum_static_macro::static_file!(root, "index.html", axum_static_macro::content_types::HTML);

async fn submit(
    mut multipart: ContentLengthLimit<Multipart, 50_000_000>,
) -> Result<(axum::http::StatusCode, String), Error> {
    let mut data = String::new();
    while let Some(field) = multipart.0.next_field().await.unwrap() {
        if field.name().ok_or(Error::FieldInvalid)? == "Contents" {
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
    let db = DB.get().ok_or_else(|| Error::NoDb)?;
    query!("INSERT INTO pastes VALUES ($1, $2, $3)", key, data, expires)
        .execute(db)
        .await?;
    Ok((
        axum::http::StatusCode::OK,
        format!("{{\"message\": \"Paste submitted!\", \"id\": \"{}\"}}", key),
    ))
}

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
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_static("text/html"),
    );
    Ok((
        axum::http::StatusCode::OK,
        headers,
        res.contents.ok_or(Error::InternalError)?,
    ))
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


enum Error {
    // Errors
    TimeError,
    NoDb,
    Sqlx(sqlx::Error),
    Multipart(axum::extract::multipart::MultipartError),
    FieldInvalid,
    InternalError,
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

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let (body, status): (std::borrow::Cow<str>, axum::http::StatusCode) = match self {
            Error::TimeError => ("Bad request".into(), axum::http::StatusCode::BAD_REQUEST),
            Error::NoDb => (
                "Database connection failed".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::FieldInvalid => (
                "HTTP field invalid".into(),
                axum::http::StatusCode::BAD_REQUEST,
            ),
            Error::InternalError => (
                "Unknown internal error".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::Sqlx(_) => (
                "Database lookup failed".into(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::Multipart(_) => (
                "MultiPartFormData invalid".into(),
                axum::http::StatusCode::BAD_REQUEST,
            ),
        };
        axum::response::Response::builder()
            .status(status)
            .body(axum::body::boxed(axum::body::Full::from(body)))
            .unwrap()
    }
}
