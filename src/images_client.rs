//! Typed image requests share the existing provider, fresh auth, and recovery.
#[cfg(test)]
#[path = "images_client_tests.rs"]
mod tests;
use super::*;
use codex_api::{ImageEditRequest, ImageGenerationRequest, ImageResponse, ImagesClient};

pub(crate) const MAX_IMAGE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

struct ImageTransport<T>(T);

impl<T: HttpTransport> HttpTransport for ImageTransport<T> {
    async fn execute(&self, mut req: Request) -> Result<Response, TransportError> {
        req.timeout = Some(Duration::from_secs(180));
        req.response_body_limit_bytes = Some(MAX_IMAGE_RESPONSE_BYTES);
        self.0.execute(req).await
    }

    async fn stream(&self, mut req: Request) -> Result<StreamResponse, TransportError> {
        req.timeout = Some(Duration::from_secs(180));
        req.response_body_limit_bytes = Some(MAX_IMAGE_RESPONSE_BYTES);
        self.0.stream(req).await
    }
}

impl Client {
    pub(crate) async fn image_request(
        &self,
        generation: ImageGenerationRequest,
        images: Option<Vec<codex_protocol::models::ImageReference>>,
    ) -> Result<(ImageResponse, Option<String>), (UpstreamError, Option<String>)> {
        let failed_id = Arc::new(Mutex::new(None));
        // Only an explicit 401 can trigger the established auth recovery. Never
        // retry timeouts or transient failures: the image may already be billed.
        run_with_unauthorized_recovery(&self.manager, &self.auth_health, |tracker| {
            let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth {
                manager: self.manager.clone(),
                tracker,
            });
            let client = ImagesClient::new(ImageTransport(self.transport.clone()), provider(), auth);
            let generation = generation.clone();
            let images = images.clone();
            let failed_id = failed_id.clone();
            async move {
                let result = if let Some(images) = images {
                    client.edit(&ImageEditRequest {
                        images,
                        prompt: generation.prompt,
                        background: generation.background,
                        model: generation.model,
                        n: generation.n,
                        quality: generation.quality,
                        size: generation.size,
                    }, HeaderMap::new()).await
                } else {
                    client.generate(&generation, HeaderMap::new()).await
                };
                result.map_err(|err| {
                    let (err, imagegen_request_id) = err.into_parts();
                    if let Ok(mut stored) = failed_id.lock() { *stored = imagegen_request_id; }
                    if matches!(&err, codex_api::ApiError::Transport(TransportError::Timeout)) {
                        return UpstreamError { status: Some(504), message: "Image request timed out; it may already have been processed. Do not retry automatically.".into(), body: None };
                    }
                    let mut error = UpstreamError::from(err);
                    error.message = "Image backend request failed".into();
                    error.body = None;
                    error
                })
            }
        }).await.map_err(|error| {
            let id = failed_id.lock().ok().and_then(|mut stored| stored.take());
            (error, id)
        })
    }
}
