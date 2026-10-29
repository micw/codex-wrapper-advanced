use super::*;
use codex_http_client::{
    HttpTransport, Request as BackendRequest, Response as BackendResponse, StreamResponse,
    TransportError,
};
use std::sync::Mutex;

fn parameters(value: Value) -> Result<ImageGenerationRequest, ()> {
    serde_json::from_value::<Parameters>(value)
        .map_err(|_| ())?
        .validate()
        .map_err(|_| ())
}

fn png_header() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes
}

#[derive(Clone)]
struct StubBackend {
    requests: Arc<Mutex<Vec<BackendRequest>>>,
    failure: Option<StatusCode>,
}

impl StubBackend {
    fn new(failure: Option<StatusCode>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            failure,
        }
    }
}

impl HttpTransport for StubBackend {
    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, TransportError> {
        self.requests.lock().unwrap().push(request);
        if let Some(status) = self.failure {
            if status == StatusCode::GATEWAY_TIMEOUT {
                return Err(TransportError::Timeout);
            }
            return Err(TransportError::Http {
                status,
                url: None,
                retry_after: None,
                headers: None,
                body: Some("private upstream diagnostic".into()),
            });
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-imagegen-request-id",
            http::HeaderValue::from_static("imagegen-test"),
        );
        Ok(BackendResponse { status: StatusCode::OK, headers, body: serde_json::to_vec(&json!({"created":1,"data":[{"b64_json":STANDARD.encode(png_header()),"generation_id":"generation-test"}]})).unwrap().into() })
    }
    async fn stream(&self, _: BackendRequest) -> Result<StreamResponse, TransportError> {
        panic!("image requests must be non-streaming")
    }
}

struct StubAuth;
impl codex_api::AuthProvider for StubAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), codex_api::AuthError> {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer test-only"),
        );
        Ok(())
    }
}

fn stub_client(transport: StubBackend) -> codex_api::ImagesClient<StubBackend> {
    codex_api::ImagesClient::new(
        transport,
        codex_api::Provider {
            name: "test".into(),
            base_url: "https://backend.invalid/codex".into(),
            query_params: None,
            headers: HeaderMap::new(),
            retry: codex_api::RetryConfig {
                max_attempts: 1,
                base_delay: std::time::Duration::from_millis(1),
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: std::time::Duration::from_secs(180),
        },
        Arc::new(StubAuth),
    )
}

#[tokio::test]
async fn typed_generation_and_edit_forward_expected_backend_json() {
    let transport = StubBackend::new(None);
    let client = stub_client(transport.clone());
    let request =
        parameters(json!({"prompt":"draw fox","n":1,"quality":"high","size":"auto"})).unwrap();
    let (response, id) = client.generate(&request, HeaderMap::new()).await.unwrap();
    assert_eq!(id.as_deref(), Some("imagegen-test"));
    assert_eq!(
        response_json(response, 1).unwrap(),
        json!({"created":1,"data":[{"b64_json":STANDARD.encode(png_header()),"generation_id":"generation-test"}]})
    );
    let inline = format!("data:image/png;base64,{}", STANDARD.encode(png_header()));
    client
        .edit(
            &codex_api::ImageEditRequest {
                images: vec![ImageReference::Inline {
                    image_url: inline.clone(),
                }],
                prompt: request.prompt.clone(),
                model: request.model.clone(),
                n: request.n,
                quality: request.quality,
                size: request.size.clone(),
                background: request.background,
            },
            HeaderMap::new(),
        )
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let expected = serde_json::to_value(&request).unwrap();
    assert_eq!(
        requests[0].url,
        "https://backend.invalid/codex/images/generations"
    );
    assert_eq!(
        requests[0]
            .body
            .as_ref()
            .and_then(codex_http_client::RequestBody::json),
        Some(&expected)
    );
    let mut expected_edit = expected;
    expected_edit["images"] = json!([{"image_url":inline}]);
    assert_eq!(
        requests[1].url,
        "https://backend.invalid/codex/images/edits"
    );
    assert_eq!(
        requests[1]
            .body
            .as_ref()
            .and_then(codex_http_client::RequestBody::json),
        Some(&expected_edit)
    );
}

#[tokio::test]
async fn image_transport_failures_and_quota_do_not_retry() {
    for status in [
        StatusCode::GATEWAY_TIMEOUT,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let transport = StubBackend::new(Some(status));
        let client = stub_client(transport.clone());
        let request = parameters(json!({"prompt":"draw fox"})).unwrap();
        assert!(client.generate(&request, HeaderMap::new()).await.is_err());
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn generation_validates_without_altering_backend_parameters() {
    let req = parameters(json!({"prompt":"draw a fox", "n":4, "quality":"medium", "background":"auto", "size":"1536x1024", "response_format":"b64_json"})).unwrap();
    assert_eq!(
        serde_json::to_value(req).unwrap(),
        json!({"prompt":"draw a fox", "model":"gpt-image-2", "n":4, "quality":"medium", "background":"auto", "size":"1536x1024"})
    );
}

#[test]
fn rejects_unknown_or_unsupported_generation_parameters() {
    for value in [
        json!({"prompt":"fox","model":"gpt-5.4"}),
        json!({"prompt":"fox","model":"gpt-image-1"}),
        json!({"prompt":"fox","n":0}),
        json!({"prompt":"fox","n":5}),
        json!({"prompt":"fox","quality":"med"}),
        json!({"prompt":"fox","size":"4096x4096"}),
        json!({"prompt":"fox","response_format":"url"}),
        json!({"prompt":"fox","output_format":"jpeg"}),
        json!({"prompt":"fox","stream":true}),
        json!({"prompt":" "}),
    ] {
        assert!(parameters(value.clone()).is_err(), "accepted {value}");
    }
}

#[test]
fn safe_response_preserves_typed_metadata_and_generation_ids() {
    let encoded = STANDARD.encode(png_header());
    let value = json!({"created":1,"data":[{"b64_json":encoded,"generation_id":"gen-1"}],"background":"opaque","quality":"high","size":"1024x1024"});
    let response: ImageResponse = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(response_json(response, 1), Ok(value));
}

#[test]
fn rejects_invalid_response_base64_magic_counts_and_dimensions() {
    for value in [
        json!({"created":1,"data":[{"b64_json":"!bad"}]}),
        json!({"created":1,"data":[{"b64_json":STANDARD.encode(b"not an image") }]}),
        json!({"created":1,"data":[]}),
        json!({"created":1,"data":[{"b64_json":STANDARD.encode(png_header())},{"b64_json":STANDARD.encode(png_header())}]}),
    ] {
        assert!(response_json(serde_json::from_value(value).unwrap(), 1).is_err());
    }
    let mut invalid = png_header();
    invalid[16..20].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(raster_mime(&invalid), None);
    assert_eq!(
        raster_mime(b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
        None
    );
}

fn state(keys: crate::listen::ApiKeys) -> AppState {
    let manager = codex_login::AuthManager::from_auth_for_testing(
        codex_login::CodexAuth::from_api_key("unused-test-token"),
    );
    let auth_health = Arc::new(AuthHealth::default());
    AppState {
        client: Client::with_auth_health(manager.clone(), auth_health.clone()),
        manager,
        auth_health,
        keys,
        metrics: Arc::new(Metrics::new()),
    }
}

#[tokio::test]
async fn both_handlers_authenticate_before_parsing_untrusted_bodies() {
    let path = std::env::temp_dir().join(format!("image-api-keys-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, "test:0123456789abcdef0123456789abcdef\n").unwrap();
    let keys = crate::listen::ApiKeys::from_file(&path).unwrap();
    // The unique, explicit path above belongs exclusively to this test.
    std::fs::remove_file(&path).unwrap();
    for endpoint in ["generations", "edits"] {
        let request = Request::builder()
            .uri(format!("/v1/images/{endpoint}"))
            .method("POST")
            .body(axum::body::Body::from("malformed body"))
            .unwrap();
        let response = if endpoint == "generations" {
            generate(State(state(keys.clone())), request).await
        } else {
            edit(State(state(keys.clone())), request).await
        };
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn multipart_rejects_masks_and_remote_references_without_backend_access() {
    for name in ["mask", "image_url", "unknown"] {
        let body = format!(
            "--testboundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\nhttps://example.invalid/picture.png\r\n--testboundary--\r\n"
        );
        let request = Request::builder()
            .uri("/v1/images/edits")
            .method("POST")
            .header("content-type", "multipart/form-data; boundary=testboundary")
            .body(axum::body::Body::from(body))
            .unwrap();
        assert_eq!(
            edit(State(state(crate::listen::ApiKeys::default())), request)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
