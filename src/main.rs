use std::{env, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use chrono::Utc;
use redis::{Commands, FromRedisValue, ToRedisArgs};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn generate_bucket_key(ip: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ip.as_bytes());
    let hash_result = hasher.finalize();
    format!("bucket:{:x}", hash_result)
}

#[derive(Serialize, Deserialize, Debug)]
struct TokenPersistence {
    tokens: i64,
    last_updated: chrono::DateTime<Utc>,
}

enum TokenPersistenceReturn {
    Okay,
    Nil,
    Token(TokenPersistence),
}

impl FromRedisValue for TokenPersistenceReturn {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        match v {
            redis::Value::BulkString(v) => Ok(TokenPersistenceReturn::Token(
                serde_json::from_slice(v).unwrap(),
            )),
            redis::Value::Nil => Ok(Self::Nil),
            redis::Value::Okay => Ok(Self::Okay),
            _ => unreachable!(),
        }
    }
}

impl ToRedisArgs for TokenPersistence {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        out.write_arg(serde_json::to_string(self).unwrap().as_bytes())
    }
}

impl TokenPersistence {
    fn new() -> Self {
        Self {
            tokens: 10,
            last_updated: Utc::now(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    redis_client: Arc<redis::Client>,
}

async fn my_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let bearer_token = match &request.headers().get("Bearer") {
        Some(t) => t.to_str().unwrap(),
        None => {
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .unwrap();
        }
    };

    let redis_key = generate_bucket_key(bearer_token);

    let token_model_result = state
        .redis_client
        .get_connection()
        .unwrap()
        .get(&redis_key)
        .unwrap_or(TokenPersistenceReturn::Token(TokenPersistence::new()));

    let token_model = match token_model_result {
        TokenPersistenceReturn::Token(tp) => tp,
        _ => TokenPersistence::new(),
    };

    let last_updated = token_model.last_updated;

    let now = Utc::now();

    let max_tokens = 10;
    let refill_rate_per_hour = 1;

    let elapsed_hours = now.signed_duration_since(last_updated).num_hours();

    let tokens_available =
        (token_model.tokens + elapsed_hours * refill_rate_per_hour).min(max_tokens);

    if tokens_available < 1 {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .body(Body::empty())
            .unwrap();
    }

    let updated_tokens = (tokens_available - 1).max(0);

    let updated_token_model = TokenPersistence {
        last_updated: now,
        tokens: updated_tokens,
    };

    let _persisted = state
        .redis_client
        .get_connection()
        .unwrap()
        .set::<&str, TokenPersistence, TokenPersistenceReturn>(&redis_key, updated_token_model)
        .unwrap();

    let response = next.run(request).await;

    response
}

#[tokio::main]
async fn main() {
    let redis_host = env::var("REDIS_HOST").unwrap_or("redis://localhost:6379".to_string());

    println!("{}", redis_host);

    let redis_client = Arc::new(redis::Client::open(redis_host).unwrap());

    let state = AppState { redis_client };

    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .layer(middleware::from_fn_with_state(state, my_middleware));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
