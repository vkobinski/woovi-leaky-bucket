use std::{env, sync::Arc};

use axum::{Router, middleware, routing::get};
use leaky_bucket::{AppState, rate_limiter_middleware};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() {
    let redis_host = env::var("REDIS_HOST").unwrap_or("redis://localhost:6379".to_string());

    println!("{}", redis_host);

    let redis_conn = Arc::new(Mutex::new(
        redis::Client::open(redis_host)
            .unwrap()
            .get_connection()
            .unwrap(),
    ));

    let state = AppState { redis_conn };

    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limiter_middleware,
        ));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
