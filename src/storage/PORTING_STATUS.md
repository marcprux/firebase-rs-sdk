## Porting status

Coverage: see the [table in the repository README](https://github.com/marcprux/firebase-rs-sdk#coverage), generated from `docs/coverage.toml`. The notes below are the porting history and are not kept in sync with that table.

==As of October 21th, 2025== 

Prompt: Compare the original JS/Typescript files in ./packages/storage and the ported files in Rust in ./src/storage, and give me an estimated guess, in percentage, of how much of the features/code of the Firebase JS SDK has been ported to Rust for this module

Thinking
I'll create a clear, concise answer with bullet points under headers for positive coverage and key gaps, referencing specific source lines like src/storage/reference.rs:81 and packages/storage/src/api.ts:129. I’ll include an estimated completeness around 60% and a friendly next-step suggestion about porting missing features to wrap it up nicely.

Storage Port Coverage

  - Core reference operations (metadata fetch/update, listing, byte downloads, multipart & resumable uploads) are
  present in Rust (src/storage/reference.rs:81, src/storage/reference.rs:94, src/storage/reference.rs:145, src/storage/
  reference.rs:195) and backed by the request builders and retry plumbing in src/storage/request/builders.rs:1.
  - Storage instance management mirrors the JS service: component registration, emulator support, bucket resolution, and
  auth/app-check header injection live in src/storage/api.rs:12 and src/storage/service.rs:1.

Key Gaps

  - JS exposes direct `ReadableStream`/service-worker piping (packages/storage/src/reference.ts:201), whereas the Rust
  port still requires callers to adapt browser streams into `AsyncRead` sources before invoking the resumable helpers.
  - (Closed on 2026-09-07.) The modular upload task with observers, pause/resume/cancel, and snapshot events
  (packages/storage/src/task.ts:62, packages/storage/src/public-types.ts:214) is now implemented in
  `src/storage/upload.rs`; what remains is the promise-like `UploadTask` (`then`/`catch`), the `StorageObserver` object
  form, and resuming a session after a process restart.
  - (Closed on 2026-09-07 for the error codes.) The Rust enum now covers the JS suite (src/storage/error.rs:1); there is
  still no token-refresh logic on 401/403 responses.
  - Estimated completion: roughly 60 % of the Firebase Storage web SDK surface has been ported to Rust—core CRUD and
  upload flows are in place, but advanced task management, streaming/string conveniences, and full error parity remain
  outstanding.

Natural next step: wire in observer-based upload tasks, add direct `ReadableStream` adapters for WASM, and continue filling
out the remaining error/streaming parity gaps.


### Development status as of 14th October 2025

- Core functionalities: Mostly implemented 
- Tests: 27 tests (passed)
- Documentation: Lacking documentation on most functions
- Examples: None provided

DISCLAIMER: This is not an official Firebase product, nor it is guaranteed that it has no bugs or that it will work as intended.

## Implemented
- Resumable upload control surface ported from `packages/storage/src/task.ts` (2026-09-07): `UploadTask` exposes
  `handle()` -> `UploadTaskHandle` (cloneable, `Send + Sync`) with `pause`, `resume`, `cancel`, `state`, `snapshot` and
  `on_state_changed`, plus `UploadTaskSnapshot` (`bytes_transferred`, `total_bytes`, `state`, `metadata`, `reference`)
  and the Web SDK `TaskState` strings (`running`, `paused`, `success`, `canceled`, `error`). Cancelling sends the
  `X-Goog-Upload-Command: cancel` command so the session is discarded server-side and no object is created, and the
  driving call fails with `storage/canceled`; a `cancelled` session status on any resumable response maps to the same
  error. `UploadTask::refresh_status` queries the session offset (`X-Goog-Upload-Size-Received`) and resyncs the task.
  Observers are notified per chunk and on every state change, never while a lock is held, so a callback may itself
  pause or cancel. `list` validates `max_results` (1..=1000) like the JS SDK. Verified against the Storage emulator by
  `storage_resumable_upload_progress_pause_and_cancel` and `storage_list_pagination_and_metadata_updates` in
  `tests/live_endpoints.rs`.
- Error mapping ported from `implementation/requests.ts`: `shared_error_handler` (401 -> `unauthenticated` / `unauthorized-app`, 402 -> `quota-exceeded`, 403 -> `unauthorized`) and `object_error_handler` (404 -> `object-not-found`) are attached to every request; listings map 404 to `bucket-not-found`; unexpected statuses are `unknown` with `status` and `server_response`; exhausted retries/timeouts are `retry-limit-exceeded`. Bucket names, object paths and download tokens are encoded like `encodeURIComponent` (previously `-`, `.` and `_` were percent-encoded). Verified against the Storage emulator by `tests/live_endpoints.rs`.

- Registered a `storage` component so apps can lazily request Storage instances, optionally keyed by bucket URL.
- Ported the location/URL parsing helpers (`Location`, path utilities, and URL detection) with unit tests that mirror the
  JavaScript behaviour for `gs://` and HTTPS endpoints.
- Stubbed a `FirebaseStorageImpl` that tracks host/bucket state, supports emulator connection, and can produce typed
  `StorageReference` values for arbitrary child paths.
- Mirrored the public Rust API façade with helpers that wrap the component container (`get_storage_for_app`,
  `storage_ref_from_storage`, etc.).
- Introduced the request/backoff scaffolding (`storage::request`) and convenience constructors on
  `FirebaseStorageImpl` so higher layers can issue HTTP calls with exponential retry policy.
- Ported the core `StorageReference` operations: metadata fetch/update, hierarchical listing (`list`/`list_all`), direct
  downloads via `get_bytes`, signed URL generation (`get_download_url`), and object deletion. Corresponding request
  builders now emit byte-download, download-URL, and delete requests with unit coverage.
- Added upload support: multipart uploads expose an async `upload_bytes` helper, and resumable uploads are modelled
  through a Rust-centric `UploadTask` that streams chunks, surfaces progress callbacks, and finalises with parsed
  metadata. Request builders for multipart/resumable flows are unit-tested with emulator-style mocks.
- Implemented string uploads (`upload_string`) plus WASM conveniences for `Blob`/`Uint8Array` sources and `get_blob`
  downloads so browser callers can mirror the Web SDK entry points without extra glue code.
- Added `upload_reader_resumable` helpers so large files can stream from any `AsyncRead` without buffering the entire
  payload, and introduced WASM adapters for `ReadableStream` sources to keep browser behaviour aligned with the Web SDK.
- Added native `get_stream` so downloads can be consumed progressively via `AsyncRead` instead of buffering everything in
  memory.
- Expanded metadata and type models: `ObjectMetadata` now tracks MD5/CRC/ETag values, parses download tokens into a
  typed collection, and exposes helpers for byte sizes. `UploadMetadata`/`SettableMetadata` provide builder-style
  ergonomics for configuring uploads and metadata updates while serialising to the REST-friendly camelCase payloads.
- Authentication and App Check headers are now injected automatically: emulator overrides feed `Authorization`
  headers, while live environments consult the Auth/App Check providers to emit `Authorization`,
  `X-Firebase-AppCheck`, `X-Firebase-Storage-Version`, and `X-Firebase-GMPID` metadata on every request.
- Unified async transport built on `reqwest::Client`, so native and wasm targets share the same retry logic while
  exposing an `async` public API.
- Added runnable examples under `examples/` (`storage_get_stream.rs`, `storage_upload_string.rs`) covering streaming
  downloads and string uploads to make the new APIs easier to adopt.

## Still To Do

1. **Token refresh & error awareness** – Now that headers are attached, add handling for auth/app-check failures by
   forcing token refreshes on 401/403 responses and mapping them to dedicated `StorageErrorCode`s.
2. **Streaming downloads** – Provide chunked/streamed download APIs (e.g. `get_stream`) so large responses don’t require
  buffering in memory when mirroring the JS SDK.
3. ~~**Task observers & snapshots**~~ – Done on 2026-09-07: `UploadTaskSnapshot`, `UploadTaskHandle::on_state_changed`
   and the full state machine (`pause`/`resume`/`cancel`) mirror the Web SDK's `state_changed` stream. Still missing: the
   promise-like `UploadTask` and recovering a session across process restarts.
4. ~~**Error parity**~~ – Done on 2026-09-07: the error module carries the full JS code suite with HTTP status mapping
   and helper constructors.
5. **Testing** – Broaden coverage with request-layer mocks, emulator integration smoke tests, and regression suites for
   the new operations.

## Next steps - Detailed completion plan

1. **Auth/App Check resiliency**
   - Teach the request pipeline to invalidate and refresh tokens when the backend returns 401/403 or the emulator hints
     at auth issues.
   - Surface distinct storage error codes for auth/app-check failures so callers can prompt users to reauthenticate.
   - When server-app support lands, read `FirebaseServerAppSettings` overrides to honour pre-provisioned tokens.
2. **Upload ergonomics**
   - Layer high-level helpers for string and stream sources on top of the new upload primitives.
   - ~~Extend `UploadTask` with pause/resume/cancel semantics~~ (done 2026-09-07); persisted session recovery (rebuilding
     a task from a stored `upload_session_url`) is still open.
3. **Observer & snapshot surface**
   - ~~Introduce `UploadTaskSnapshot` plus observer callbacks that mirror the Web SDK~~ (done 2026-09-07). The
     `StorageObserver` object form (next/error/complete) is still closure-only.
   - ~~Add unit coverage for observer registration and state transitions~~ (done 2026-09-07, `src/storage/upload.rs`
     tests plus two emulator tests).
