# Changelog

## 1.12.2

- Add `gpt-6.2-sol:long` to `/v1/models` with the official pinned Codex catalogue's declared 872,000-token maximum; the base model keeps its declared 500,000-token default. These are provider metadata, not a new live wrapper measurement.
- Keep both aliases routed to `gpt-6.2-sol` with identical reasoning levels and unchanged request effort. Add an explicit wire budget variant for GPT-6.2 Sol only, with both context values set to the declared maximum and the base model's visibility/modalities/reasoning; retain the base wire entry with both budgets. Older Sol wire entries and Astra get no new alias, and no long alias is emitted without declared headroom.
- Add offline alias/context regression coverage; preserve the 1.12.1 modality ceiling, Codex pins, dependencies, and authentication behavior.

## 1.12.1

- Limit projected `/wire/v1/models` input modalities to the intersection of the backend declaration and the wrapper-supported text/image surface. Do not advertise audio or unknown future modalities, and do not invent absent capabilities.
- Add offline regression coverage for GPT-6.2 Sol's live text/image/audio metadata while preserving its 500,000/872,000 context values and reasoning metadata; keep Astra unchanged.
- Leave raw catalogue diagnostics, permissive wire/Responses transport, authentication, Codex pins, dependencies, and long-context aliases unchanged. Raw pass-through does not imply support for all backend modalities across wrapper surfaces.

## 1.12.0

- Upgrade the pinned official Codex libraries and model-catalogue client version to stable 0.169.1, including matching websocket fork pins and lockfile.
- Support discovery of GPT-6.2 Sol and Astra without inventing `none` reasoning support or unmeasured long-context aliases; keep context capabilities backend-driven and the default request model unchanged.
- Adapt the upstream async authentication API while preserving session headers, bounded auth recovery, and existing image request/output semantics.
- Rename the wrapper credential transaction lock to `wrapper-auth.lock` to avoid self-deadlock with upstream's new internal `auth.lock`. **Upgrading from <=1.11 requires a drained stop-before-start transition; do not overlap old/new processes sharing credentials.** See [UPSTREAM-UPGRADE.md](UPSTREAM-UPGRADE.md).
- Add keyless GPT-6.2 catalogue and raster serialization regression coverage.

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
