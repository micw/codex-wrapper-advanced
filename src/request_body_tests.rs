use axum::http::HeaderValue;
use bytes::Bytes;
use serde_json::Value;
use serde_json::json;

use super::*;

#[test]
fn identity_and_zstd_decode_to_the_same_json() {
    let expected = json!({"message": "hello", "items": [1, 2, 3]});
    let raw = serde_json::to_vec(&expected).unwrap();
    let identity = decode_json::<Value>(&HeaderMap::new(), Bytes::from(raw.clone()), 1024).unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_ENCODING,
        HeaderValue::from_static("zstd"),
    );
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3).unwrap();
    let zstd = decode_json::<Value>(&headers, Bytes::from(compressed), 1024).unwrap();

    assert_eq!(identity.value, expected);
    assert_eq!(zstd.value, expected);
    assert!(!identity.compressed);
    assert!(zstd.compressed);
    assert_eq!(identity.decoded_bytes, zstd.decoded_bytes);
}

#[test]
fn decoded_size_is_bounded() {
    let raw = serde_json::to_vec(&json!({"message": "long enough"})).unwrap();
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_ENCODING,
        HeaderValue::from_static("zstd"),
    );

    let error = decode_json::<Value>(&headers, Bytes::from(compressed), 4).unwrap_err();

    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn unknown_content_encoding_is_rejected() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_ENCODING,
        HeaderValue::from_static("gzip"),
    );

    let error = decode_json::<Value>(&headers, Bytes::from_static(b"{}"), 1024).unwrap_err();

    assert_eq!(error.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}
