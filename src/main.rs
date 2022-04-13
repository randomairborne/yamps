use axum::extract::{RequestParts, Path};
use axum::routing::{get, post};
use oncecell::OnceCell;

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
    let mut config = toml::from_str::<Config>(&config_string).expect("Failed to parse config");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(config.db)
        .await?;
    migrate!("migrations").run(&pool).await.unwrap();
    let app = axum::Router::new()
        .route("/", get(root))
        .route("/:path", get(getpaste))
        .route("/submit", post(submit));
    tokio::spawn(async move {delete_expired().await});
    axum::Server::bind(&std::net::SocketAddr::from(([127, 0, 0, 1], config.port)))
        .serve(app.into_make_service())
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for ctrl+c");
        })
        .await
        .expect("Failed to bind to address, is something else using the port?");
}

static_file!(root, "root.html", axum_static_file::content_types::HTML);

async fn submit(RequestParts(req): RequestParts<B> {
    let persistence_length = chrono::Duration::weeks(1);
    let expired = chrono::offset::Local::now().checked_add_signed().ok_or(Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, r#"{"error": "This should not have happened"}"#.to_string())))?;
    let key = random_string::generate(8, "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz1234567890-");
    // TODO error check this
    sqlx::query!("INSERT INTO pastes VALUES ($1, $2, $3)", key, req.body(), expires).execute().await.unwrap()
    Ok((axum::http::StatusCode::OK, format!("{{\"message\": \"Paste submitted!\", \"id\": \"{}\"}}", key)))
}

async fn getpaste(Path(id): Path<String>) {

}

async fn delete_expired() {
    
}

enum Error {
    
}
