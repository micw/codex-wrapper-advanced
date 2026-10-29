use super::map_events;
use crate::wire::Event;
use codex_api::ApiError;
use futures::StreamExt;
use std::time::Duration;

async fn failure(error: ApiError) -> serde_json::Value {
    let events = map_events(futures::stream::iter([Err(error)]))
        .collect::<Vec<Event>>()
        .await;
    assert_eq!(events.len(), 1);
    serde_json::to_value(&events[0]).expect("wire failure serializes")
}

#[tokio::test]
async fn structured_retryable_processing_error_remains_retryable_on_the_wire() {
    let message = "An error occurred while processing your request. You can retry your request.";
    assert_eq!(
        failure(ApiError::Retryable {
            message: message.into(),
            retry_after: codex_http_client::RetryAfter::from_delay(Duration::from_secs(2)),
        })
        .await,
        serde_json::json!({
            "type": "failed",
            "message": format!("retryable error: {message}"),
            "retryable": true,
        })
    );
}

#[tokio::test]
async fn structured_rate_limit_delay_remains_retryable_without_a_fabricated_http_status() {
    assert_eq!(
        failure(ApiError::RateLimitExceeded {
            message: "temporarily rate limited".into(),
            retry_after: codex_http_client::RetryAfter::from_delay(Duration::from_secs(3)),
        })
        .await,
        serde_json::json!({
            "type": "failed",
            "message": "rate limit exceeded: temporarily rate limited",
            "retryable": true,
        })
    );
}

#[tokio::test]
async fn policy_authentication_and_invalid_requests_stay_terminal() {
    for error in [
        ApiError::CyberPolicy {
            message: "blocked".into(),
        },
        ApiError::BioPolicy {
            message: "blocked".into(),
        },
        ApiError::InvalidRequest {
            message: "invalid model".into(),
        },
        ApiError::Api {
            status: http::StatusCode::UNAUTHORIZED,
            message: "unauthorized".into(),
        },
        ApiError::Stream("retryable error: arbitrary text must not grant retryability".into()),
    ] {
        let message = match &error {
            ApiError::Api { message, .. } => message.clone(),
            ApiError::InvalidRequest { message } => message.clone(),
            other => other.to_string(),
        };
        assert_eq!(
            failure(error).await,
            serde_json::json!({ "type": "failed", "message": message, "retryable": false })
        );
    }
}

#[tokio::test]
async fn existing_retryable_http_statuses_keep_their_wire_classification() {
    for status in [
        http::StatusCode::TOO_MANY_REQUESTS,
        http::StatusCode::SERVICE_UNAVAILABLE,
    ] {
        assert_eq!(
            failure(ApiError::Api {
                status,
                message: "temporary".into()
            })
            .await,
            serde_json::json!({ "type": "failed", "message": "temporary", "retryable": true })
        );
    }
}
