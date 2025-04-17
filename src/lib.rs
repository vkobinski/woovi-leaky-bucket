use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use chrono::Utc;
use redis::{ConnectionLike, FromRedisValue, RedisError, ToRedisArgs};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

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
            redis::Value::Array(v) if v.len() == 1 => {
                TokenPersistenceReturn::from_redis_value(&v[0])
            }
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

pub struct AppState<C>
where
    C: ConnectionLike + Send + Sync + 'static,
{
    pub redis_conn: Arc<Mutex<C>>,
}

impl<C> Clone for AppState<C>
where
    C: ConnectionLike + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            redis_conn: Arc::clone(&self.redis_conn),
        }
    }
}

pub async fn rate_limiter_middleware<C>(
    State(state): State<AppState<C>>,
    request: Request,
    next: Next,
) -> Response
where
    C: ConnectionLike + Send + Sync + 'static,
{
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

    let mut conn = state.redis_conn.lock().await;

    let transaction = redis::transaction(&mut *conn, &[&redis_key], |con, pipe| {
        let token_model_result = pipe
            .get(&redis_key)
            .query(con)
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
            return Err(RedisError::from((
                redis::ErrorKind::ClientError,
                "Too many requests",
            )));
        }

        let updated_tokens = (tokens_available - 1).max(0);

        let updated_token_model = TokenPersistence {
            last_updated: now,
            tokens: updated_tokens,
        };

        let _ = pipe
            .set(&redis_key, updated_token_model)
            .ignore()
            .query::<TokenPersistenceReturn>(con);

        Ok(Some(()))
    });

    dbg!(&transaction);

    match transaction {
        Err(e) => {
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::empty())
                .unwrap();
        }
        _ => {}
    }

    let response = next.run(request).await;

    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, Response, StatusCode},
        middleware,
    };
    use chrono::Utc;
    use redis::{Value, cmd};
    use redis_test::{MockCmd, MockRedisConnection};
    use tokio::sync::Mutex;
    use tower::{ServiceBuilder, ServiceExt};

    use crate::{AppState, TokenPersistence, generate_bucket_key, rate_limiter_middleware};

    #[tokio::test]
    async fn test_rate_limiter_allows_request_via_servicebuilder() {
        let starting = TokenPersistence::new();
        let json = serde_json::to_string(&starting).unwrap();

        let mock = MockRedisConnection::new(vec![
            MockCmd::new(
                cmd("WATCH").arg(generate_bucket_key("127.0.0.1")),
                Ok(Value::Okay),
            ),
            MockCmd::new(cmd("MULTI"), Ok(Value::Okay)),
            MockCmd::new(
                cmd("GET").arg(generate_bucket_key("127.0.0.1")),
                Ok(Value::Nil),
            ),
            MockCmd::new(cmd("UNWATCH"), Ok(Value::Okay)),
            MockCmd::new(
                cmd("SET")
                    .arg(generate_bucket_key("127.0.0.1"))
                    .arg(json.clone().to_string()),
                Ok(Value::Okay),
            ),
        ]);
        let state = AppState {
            redis_conn: Arc::new(Mutex::new(mock.clone())),
        };

        let inner = tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::empty())
                    .unwrap(),
            )
        });

        let svc = ServiceBuilder::new()
            .layer(middleware::from_fn_with_state(
                state.clone(),
                rate_limiter_middleware::<MockRedisConnection>,
            ))
            .service(inner);

        let response = svc
            .oneshot(
                Request::builder()
                    .header("Bearer", "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_rate_limiter_denies_request() {
        let mut starting = TokenPersistence::new();
        starting.tokens = 0;
        let json = serde_json::to_string(&starting).unwrap();

        let mock = MockRedisConnection::new(vec![
            MockCmd::new(
                cmd("WATCH").arg(generate_bucket_key("127.0.0.1")),
                Ok(Value::Okay),
            ),
            MockCmd::new(cmd("MULTI"), Ok(Value::Okay)),
            MockCmd::new(
                cmd("GET").arg(generate_bucket_key("127.0.0.1")),
                Ok(json.clone().to_string()),
            ),
        ]);
        let state = AppState {
            redis_conn: Arc::new(Mutex::new(mock.clone())),
        };

        let inner = tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::empty())
                    .unwrap(),
            )
        });

        let svc = ServiceBuilder::new()
            .layer(middleware::from_fn_with_state(
                state.clone(),
                rate_limiter_middleware::<MockRedisConnection>,
            ))
            .service(inner);

        let response = svc
            .oneshot(
                Request::builder()
                    .header("Bearer", "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
