//! OpenAI image HTTP surface. No remote references or hidden retries.
use super::*;
use axum::extract::{FromRequest, Multipart, Request};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use codex_api::{ImageBackground, ImageGenerationRequest, ImageQuality, ImageResponse};
use codex_protocol::models::ImageReference;
use serde::Deserialize;
use serde_json::Value;

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_REFERENCES: usize = 5;

type HttpResponse = axum::response::Response;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    prompt: String,
    model: Option<String>,
    background: Option<ImageBackground>,
    n: Option<u64>,
    quality: Option<ImageQuality>,
    size: Option<String>,
    response_format: Option<String>,
}

fn invalid(message: &str) -> HttpResponse {
    error(
        StatusCode::BAD_REQUEST,
        message,
        "invalid_request_error",
        Some("invalid_image_request"),
    )
}

impl Parameters {
    fn validate(self) -> Result<ImageGenerationRequest, &'static str> {
        let model = self.model.unwrap_or_else(|| "gpt-image-2".into());
        if model != "gpt-image-2" {
            return Err("Only gpt-image-2 is supported by the image endpoints");
        }
        if self.prompt.trim().is_empty() {
            return Err("prompt must not be empty");
        }
        if self.n.is_some_and(|n| !(1..=4).contains(&n)) {
            return Err("n must be between 1 and 4");
        }
        if self
            .response_format
            .as_deref()
            .is_some_and(|f| f != "b64_json")
        {
            return Err("Only response_format=b64_json is supported");
        }
        if self
            .size
            .as_deref()
            .is_some_and(|size| !matches!(size, "auto" | "1024x1024" | "1536x1024" | "1024x1536"))
        {
            return Err("size must be auto, 1024x1024, 1536x1024, or 1024x1536");
        }
        Ok(ImageGenerationRequest {
            prompt: self.prompt,
            model,
            background: self.background,
            n: self.n,
            quality: self.quality,
            size: self.size,
        })
    }
}

pub(super) async fn generate(State(state): State<AppState>, request: Request) -> HttpResponse {
    if state.keys.authenticate(request.headers()).is_none() {
        return unauthorized();
    }
    if !request
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
    {
        return invalid("Image generation requires application/json");
    }
    let headers = request.headers().clone();
    let body = match Bytes::from_request(request, &state).await {
        Ok(body) => body,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Image request body exceeds the limit",
                "invalid_request_error",
                None,
            );
        }
    };
    let params = match request_json::<Parameters>(&headers, body) {
        Ok(decoded) => decoded.value,
        Err(response) => return *response,
    };
    let generation = match params.validate() {
        Ok(req) => req,
        Err(err) => return invalid(err),
    };
    dispatch(&state.client, generation, None).await
}

pub(super) async fn edit(State(state): State<AppState>, request: Request) -> HttpResponse {
    if state.keys.authenticate(request.headers()).is_none() {
        return unauthorized();
    }
    let mut multipart = match Multipart::from_request(request, &state).await {
        Ok(value) => value,
        Err(_) => {
            return invalid(
                "Image edits require multipart/form-data with uploaded image or image[] fields",
            );
        }
    };
    let mut params = serde_json::Map::new();
    let mut images = Vec::new();
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => {
                return error(
                    err.status(),
                    "Invalid or oversized multipart body",
                    "invalid_request_error",
                    None,
                );
            }
        };
        let name = field.name().unwrap_or("").to_owned();
        let is_image = matches!(name.as_str(), "image" | "image[]");
        if !is_image
            && !matches!(
                name.as_str(),
                "prompt" | "model" | "background" | "n" | "quality" | "size" | "response_format"
            )
        {
            return invalid(
                "Unsupported multipart field (masks, remote references and additional parameters are not supported)",
            );
        }
        if is_image && images.len() >= MAX_REFERENCES {
            return invalid("At most five reference images are supported");
        }
        if !is_image && params.contains_key(&name) {
            return invalid("Duplicate multipart parameter");
        }
        let limit = if is_image {
            MAX_IMAGE_BYTES
        } else {
            1024 * 1024
        };
        let mut bytes = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= limit => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(Some(_)) => {
                    return error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Multipart field exceeds the size limit",
                        "invalid_request_error",
                        None,
                    );
                }
                Ok(None) => break,
                Err(err) => {
                    return error(
                        err.status(),
                        "Invalid or oversized multipart body",
                        "invalid_request_error",
                        None,
                    );
                }
            }
        }
        if is_image {
            let Some(mime) = raster_mime(&bytes) else {
                return invalid("Images must contain PNG, JPEG, WebP or GIF raster data");
            };
            images.push(ImageReference::Inline {
                image_url: format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
            });
        } else {
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => return invalid("Multipart parameters must be UTF-8"),
            };
            let value = if name == "n" {
                match text.parse::<u64>() {
                    Ok(n) => json!(n),
                    Err(_) => return invalid("n must be an integer"),
                }
            } else {
                json!(text)
            };
            params.insert(name, value);
        }
    }
    if images.is_empty() {
        return invalid("At least one uploaded image is required");
    }
    let params: Parameters = match serde_json::from_value(Value::Object(params)) {
        Ok(params) => params,
        Err(_) => return invalid("Missing or invalid image edit parameters"),
    };
    let generation = match params.validate() {
        Ok(req) => req,
        Err(err) => return invalid(err),
    };
    dispatch(&state.client, generation, Some(images)).await
}

async fn dispatch(
    client: &Client,
    generation: ImageGenerationRequest,
    images: Option<Vec<ImageReference>>,
) -> HttpResponse {
    let expected = generation.n.unwrap_or(1) as usize;
    match client.image_request(generation, images).await {
        Ok((response, upstream_id)) => {
            let body = match response_json(response, expected) {
                Ok(body) => body,
                Err(()) => {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "Invalid or oversized image backend response",
                        "server_error",
                        None,
                    );
                }
            };
            let mut response = Json(body).into_response();
            response.headers_mut().insert(
                "x-request-id",
                http::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())
                    .expect("UUID is a valid header"),
            );
            if let Some(value) = upstream_id.and_then(|id| http::HeaderValue::from_str(&id).ok()) {
                response
                    .headers_mut()
                    .insert("x-codex-imagegen-request-id", value);
            }
            response
        }
        Err((err, upstream_id)) => {
            let status = err
                .status
                .and_then(|s| StatusCode::from_u16(s).ok())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let message = if status == StatusCode::GATEWAY_TIMEOUT {
                "Image request timed out; it may already have been processed. Do not retry automatically."
            } else {
                "Image backend request failed"
            };
            let mut response = error(status, message, "server_error", None);
            response.headers_mut().insert(
                "x-request-id",
                http::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())
                    .expect("UUID is a valid header"),
            );
            if let Some(value) = upstream_id.and_then(|id| http::HeaderValue::from_str(&id).ok()) {
                response
                    .headers_mut()
                    .insert("x-codex-imagegen-request-id", value);
            }
            response
        }
    }
}

fn response_json(response: ImageResponse, expected: usize) -> Result<Value, ()> {
    if response.data.len() != expected || response.data.is_empty() || response.data.len() > 4 {
        return Err(());
    }
    let mut total = 0usize;
    let mut data = Vec::new();
    for image in response.data {
        if image.b64_json.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err(());
        }
        let decoded = STANDARD.decode(&image.b64_json).map_err(|_| ())?;
        total = total.checked_add(decoded.len()).ok_or(())?;
        if decoded.len() > MAX_IMAGE_BYTES
            || total > MAX_TOTAL_IMAGE_BYTES
            || raster_mime(&decoded).is_none()
        {
            return Err(());
        }
        let mut item = json!({"b64_json": image.b64_json});
        if let Some(id) = image.generation_id {
            item["generation_id"] = json!(id);
        }
        data.push(item);
    }
    let mut value = json!({"created": response.created, "data": data});
    if let Some(background) = response.background {
        value["background"] = json!(background);
    }
    if let Some(quality) = response.quality {
        value["quality"] = json!(quality);
    }
    if let Some(size) = response.size {
        value["size"] = json!(size);
    }
    // Typed upstream fields only; never fabricate usage or output format.
    if serde_json::to_vec(&value).map_err(|_| ())?.len() > 32 * 1024 * 1024 {
        return Err(());
    }
    Ok(value)
}

/// Header checks only, not a full image decoder. Never fetches external data.
fn raster_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return dimensions_ok(width, height).then_some("image/png");
    }
    if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return dimensions_ok(width, height).then_some("image/gif");
    }
    if bytes.len() >= 20
        && bytes.starts_with(b"RIFF")
        && &bytes[8..12] == b"WEBP"
        && matches!(&bytes[12..16], b"VP8 " | b"VP8L" | b"VP8X")
    {
        let (width, height) = match &bytes[12..16] {
            b"VP8X" if bytes.len() >= 30 => {
                let read24 =
                    |v: &[u8]| u32::from(v[0]) | (u32::from(v[1]) << 8) | (u32::from(v[2]) << 16);
                (read24(&bytes[24..27]) + 1, read24(&bytes[27..30]) + 1)
            }
            b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
                let bits = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            b"VP8 " if bytes.len() >= 30 && bytes[23..26] == [0x9d, 0x01, 0x2a] => (
                u32::from(u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3fff),
                u32::from(u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3fff),
            ),
            _ => return None,
        };
        return dimensions_ok(width, height).then_some("image/webp");
    }
    if bytes.len() >= 4 && bytes.starts_with(b"\xff\xd8\xff") && bytes.ends_with(b"\xff\xd9") {
        let mut offset = 2usize;
        while offset + 4 <= bytes.len() {
            if bytes[offset] != 0xff {
                return None;
            }
            while bytes.get(offset) == Some(&0xff) {
                offset += 1;
            }
            let marker = *bytes.get(offset)?;
            offset += 1;
            if matches!(marker, 0xd9 | 0xda) {
                return None;
            }
            if matches!(marker, 0x01 | 0xd0..=0xd7) {
                continue;
            }
            let length =
                u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as usize;
            if length < 2 || offset.checked_add(length)? > bytes.len() {
                return None;
            }
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                if length < 8 {
                    return None;
                }
                let height =
                    u16::from_be_bytes(bytes[offset + 3..offset + 5].try_into().ok()?) as u32;
                let width =
                    u16::from_be_bytes(bytes[offset + 5..offset + 7].try_into().ok()?) as u32;
                return dimensions_ok(width, height).then_some("image/jpeg");
            }
            offset += length;
        }
    }
    None
}

fn dimensions_ok(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= 32768
        && height <= 32768
        && u64::from(width) * u64::from(height) <= 100_000_000
}

#[cfg(test)]
#[path = "image_api_tests.rs"]
mod tests;
