# Changelog

## 1.11.0

- Add authenticated OpenAI-compatible `POST /v1/images/generations` and multipart `POST /v1/images/edits`, using the official Codex Images client and existing subscription login.
- Support the statically declared `gpt-image-2` route, base64 image responses, bounded raster uploads/results, and explicit parameter validation. Masks, remote image references, URL output, and streaming image responses are not supported.
- Leave `/v1/models` and wire v1 unchanged: the model catalog remains chat-only; there is no image-discovery endpoint.
- Do not automatically retry image requests after timeouts, transport failures, or server errors. An explicit authentication rejection may refresh credentials through the existing recovery path.
- Update the pinned Codex libraries to 0.165.0, Rust to 1.99.1, and compatible dependency versions in a separate dependency commit. Preserve upstream Rama alpha versions required for API compatibility and use upstream plan display names.
- Add offline request serialization, authorization, input/output bounds, and transport retry regression coverage.

## 1.10.2

- Preserve structured upstream `ApiError::Retryable` and `RateLimitExceeded` errors as wire `failed` events with `retryable: true`. Previously these errors could lose their classification during display-message conversion and incorrectly appear terminal to consumers.
- Keep retry execution with the consumer: this release does not introduce hidden wrapper retries or change the wire schema. Existing HTTP 429/503 handling remains retryable; authentication, policy, invalid-request and arbitrary stream errors remain terminal.
- Add regression coverage through the actual upstream-error-to-wire-event stream translation, including misleading retry-related text that must not change classification.

This fixes missing consumer retries, not the underlying frequency of upstream processing errors. Retry delays embedded in these upstream variants are not added to wire v1 by this patch.
