//! JSON request decoding, including optional Zstandard content encoding.

use std::io::Read as _;
use std::time::Instant;

use axum::http::HeaderMap;
use axum::http::StatusCode;
use bytes::Bytes;
use serde::de::DeserializeOwned;

#[derive(Debug)]
pub(crate) struct DecodedJson<T> {
    pub value: T,
    pub encoded_bytes: usize,
    pub decoded_bytes: usize,
    pub decode_ms: f64,
    pub compressed: bool,
}

#[derive(Debug)]
pub(crate) struct DecodeError {
    pub status: StatusCode,
    pub message: String,
}

pub(crate) fn decode_json<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: Bytes,
    max_decoded_bytes: usize,
) -> Result<DecodedJson<T>, DecodeError> {
    let started = Instant::now();
    let encoded_bytes = body.len();
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|err| DecodeError {
            status: StatusCode::BAD_REQUEST,
            message: format!("invalid Content-Encoding header: {err}"),
        })?
        .unwrap_or("identity")
        .trim();

    let (decoded, compressed) = if encoding.eq_ignore_ascii_case("identity") || encoding.is_empty()
    {
        (body.to_vec(), false)
    } else if encoding.eq_ignore_ascii_case("zstd") {
        let decoder =
            zstd::stream::read::Decoder::new(body.as_ref()).map_err(|err| DecodeError {
                status: StatusCode::BAD_REQUEST,
                message: format!("invalid zstd request body: {err}"),
            })?;
        let mut decoded = Vec::new();
        decoder
            .take(max_decoded_bytes.saturating_add(1) as u64)
            .read_to_end(&mut decoded)
            .map_err(|err| DecodeError {
                status: StatusCode::BAD_REQUEST,
                message: format!("invalid zstd request body: {err}"),
            })?;
        (decoded, true)
    } else {
        return Err(DecodeError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!(
                "unsupported Content-Encoding {encoding:?}; expected identity or zstd"
            ),
        });
    };

    if decoded.len() > max_decoded_bytes {
        return Err(DecodeError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("decoded request body exceeds {max_decoded_bytes} bytes"),
        });
    }
    let value = serde_json::from_slice(&decoded).map_err(|err| DecodeError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid JSON request body: {err}"),
    })?;
    Ok(DecodedJson {
        value,
        encoded_bytes,
        decoded_bytes: decoded.len(),
        decode_ms: started.elapsed().as_secs_f64() * 1000.0,
        compressed,
    })
}

#[cfg(test)]
#[path = "request_body_tests.rs"]
mod tests;
