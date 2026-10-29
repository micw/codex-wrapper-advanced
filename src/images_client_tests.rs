use super::*;

#[derive(Clone, Default)]
struct CaptureTransport(Arc<Mutex<Vec<Request>>>);

impl HttpTransport for CaptureTransport {
    async fn execute(&self, request: Request) -> Result<Response, TransportError> {
        self.0.lock().unwrap().push(request);
        Err(TransportError::Timeout)
    }
    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        self.0.lock().unwrap().push(request);
        Err(TransportError::Timeout)
    }
}

#[tokio::test]
async fn production_image_transport_caps_both_paths_before_delegating() {
    let capture = CaptureTransport::default();
    let transport = ImageTransport(capture.clone());
    let mut request = Request::new(
        Method::POST,
        "https://backend.invalid/images/generations".into(),
    );
    request.timeout = Some(Duration::from_secs(900));
    request.response_body_limit_bytes = None;
    assert!(matches!(
        transport.execute(request.clone()).await,
        Err(TransportError::Timeout)
    ));
    assert!(matches!(
        transport.stream(request).await,
        Err(TransportError::Timeout)
    ));
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(
            (request.timeout, request.response_body_limit_bytes),
            (Some(Duration::from_secs(180)), Some(32 * 1024 * 1024))
        );
    }
}
