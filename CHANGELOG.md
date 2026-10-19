# Changelog

## 1.10.2

- Preserve structured upstream `ApiError::Retryable` and `RateLimitExceeded` errors as wire `failed` events with `retryable: true`. Previously these errors could lose their classification during display-message conversion and incorrectly appear terminal to consumers.
- Keep retry execution with the consumer: this release does not introduce hidden wrapper retries or change the wire schema. Existing HTTP 429/503 handling remains retryable; authentication, policy, invalid-request and arbitrary stream errors remain terminal.
- Add regression coverage through the actual upstream-error-to-wire-event stream translation, including misleading retry-related text that must not change classification.

This fixes missing consumer retries, not the underlying frequency of upstream processing errors. Retry delays embedded in these upstream variants are not added to wire v1 by this patch.
