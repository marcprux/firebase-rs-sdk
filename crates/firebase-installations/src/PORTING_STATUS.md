## Porting status
Coverage: see the [table in the repository README](https://github.com/marcprux/firebase-rs-sdk#coverage), generated from `docs/coverage.toml`. The notes below are the porting history and are not kept in sync with that table.

==As of October 20th, 2025==

Prompt: Compare the original JS/Typescript files in ./packages/installations and the ported files in Rust in ./src/installations, and give me an estimated guess, in percentage, of how much of the features/code of the Firebase JS SDK has been ported to Rust

I’m reviewing the Rust and JS implementations to compare their feature coverage. The Rust code now performs real Installation registration and token refresh via the REST API, replacing the previous stub. Persistence, concurrency coordination, and secondary APIs (delete, onIdChange) still need to be ported. Given the new network integration, coverage increases from ~5 % to roughly 15 %. I’ll reference specific file paths and lines to support this analysis and highlight the biggest gaps before crafting the final summary.


  - Network registration and token refresh mirror the JS helpers (`packages/installations/src/functions/create-installation-request.ts`, `generate-auth-token-request.ts`) through the shared HTTP client (`src/rest/mod.rs:1`).
  - `get_id` and `get_token` call through the REST client and cache the resulting FID/token pair like the JS `getInstallationEntry` flow (`src/installations/api.rs:23`).
  - Configuration extraction (`src/installations/config.rs:6`) validates required app options, similar to JS `extractAppConfig`.
  - Installation entries (FID + refresh/auth tokens) persist to disk between runs using the default file-backed cache (`src/installations/persistence.rs:1`).
  - `delete_installations` mirrors the JS delete flow, issuing the REST delete call and clearing cached state (`src/installations/api.rs:200`).
  - onIdChange callbacks, IndexedDB-style concurrency guards, richer retry/backoff policies, and emulator/diagnostics features remain outstanding.


## Implemented
- `get_token` drops the local entry and re-registers when the backend answers 401/404 for `authTokens:generate` (JS `refreshAuthToken` behaviour); `InstallationsError::server_code()` exposes the HTTP status.
- Component registration exposing `get_installations` with per-app caching (`src/installations/api.rs:146`).
- App config extraction and validation mirroring the JS helper (`src/installations/config.rs:6`).
- Async REST client shared by both targets, on `firebase_core::platform::http` (`src/rest/mod.rs:1`).
- `Installations` public/internal APIs are now async, performing registration, token refresh, and delete operations without blocking (`src/installations/api.rs:112`).
- File-backed persistence for native targets and IndexedDB + BroadcastChannel-backed persistence for wasm builds, including wasm-bindgen tests that verify round-trips when `wasm-web` and `experimental-indexed-db` are enabled (`src/installations/persistence.rs`).
- Internal helper that surfaces the full installation entry (FID, refresh token, auth token) for other modules such as Messaging (`src/installations/api.rs:185`).
- Unit tests covering config validation, async REST flows, persistence round-trips, delete behaviour, and service behaviour for forced refreshes (`src/installations/rest/tests.rs:1`, `src/installations/api.rs:472`, `src/installations/persistence.rs:80`).
- Private `installations-internal` component provides shared `get_id`/`get_token` helpers (`src/installations/api.rs:210`).
- `Installations::on_id_change` exposes the JS `onIdChange` listener semantics, returning an unsubscribe handle and notifying when new Installation IDs are registered (`src/installations/api.rs`).


### WASM Notes

- Enable the `wasm-web` feature to pull in the fetch-based REST client and browser-specific glue.
- Add `experimental-indexed-db` alongside `wasm-web` to persist installations to IndexedDB; without it, wasm builds fall back to in-memory persistence while keeping the same API surface.
- IndexedDB persistence now has wasm-bindgen tests that validate round-trip storage and BroadcastChannel propagation (`src/installations/persistence.rs`).


## Still to do
- Add concurrency coordination and migrations for the persistence layer (IndexedDB-style pending markers, multi-process guards).
- Implement additional JS parity APIs (heartbeat headers, emulator tooling, diagnostics hooks) and surface richer telemetry for downstream modules.
- Add ETag handling, heartbeat/X-Firebase-Client integration, and exponential backoff policies for REST requests.
- Provide emulator support, diagnostics logging, and richer error mapping from REST responses.
- Expand integration tests and shared fixtures to cover retry paths and error propagation.

## Next steps - Detailed completion plan
1. **Document & test the shared messaging flow**
   - Add an example (and wasm doc snippets) that shows messaging awaiting `InstallationsInternal::get_installation_entry` before contacting FCM.
   - Port the key `token-manager` tests from the JS SDK to ensure retry/backoff behaviour matches expectations.
2. **Strengthen persistence coordination**
   - Extend the new pending-registration markers with structured telemetry/backoff so repeated failures remain observable.
   - Add retry/backoff policies on IndexedDB opening failures and consider exposing metrics to consumers.
3. **Follow-on parity work**
   - Revisit JS parity items (heartbeat headers, emulator tooling) once the messaging integration settles.
   - Expand structured logging and diagnostics so native and wasm targets surface actionable errors to consuming modules.
