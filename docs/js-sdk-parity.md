# Where this SDK stands against the JavaScript SDK

The Firebase JavaScript SDK is the reference implementation this port follows: every module here
cites the `packages/**` file it was ported from, and the coverage table in the README is measured
against it. This document is the study behind those numbers — what the JS SDK actually contains,
which parts of it exist here, which parts are deliberately different, and which differences are
bugs waiting to be fixed.

Measured against **firebase-js-sdk v12.18.0** (`b70f27b`, 2026-09-08).

```bash
git clone --depth 1 https://github.com/firebase/firebase-js-sdk
scripts/api_parity.py ../firebase-js-sdk            # the table below
scripts/api_parity.py ../firebase-js-sdk --missing  # every entity with no counterpart
```

## The shape of the thing being ported

The JS SDK is ~180k lines of TypeScript across 14 product packages plus `app`, `component`,
`logger` and `util`. It is not evenly distributed: Firestore alone is 82k lines — more than this
entire crate — and most of that is not the API surface but the machinery underneath it.

| JS package | TS lines | Rust crate | Rust lines |
|---|--:|---|--:|
| firestore | 81,586 | firebase-firestore | 15,769 |
| database | 21,411 | firebase-database | 6,043 |
| auth | 20,926 | firebase-auth | 12,351 |
| ai | 9,612 | firebase-ai | 1,847 |
| storage | 6,382 | firebase-storage | 4,831 |
| data-connect | 5,619 | firebase-data-connect | 1,848 |
| messaging | 4,905 | firebase-messaging | 2,530 |
| remote-config | 3,575 | firebase-remote-config | 2,620 |
| analytics | 2,776 | firebase-analytics | 1,053 |
| performance | 2,602 | firebase-performance | 1,972 |
| app-check | 2,579 | firebase-app-check | 3,861 |
| app + component + logger + util | 5,821 | firebase-core | 7,226 |
| installations | 2,041 | firebase-installations | 2,197 |
| functions | 1,551 | firebase-functions | 1,461 |

Line counts are a blunt instrument, but the pattern in them is real: where a product is mostly a
REST client (installations, functions, storage, app-check) the Rust is the same size or larger,
because Rust spells out what TypeScript infers. Where a product is an offline engine (Firestore,
Database), the Rust is a fraction of the size — that is the missing machinery, not tighter code.

## Public API parity

The JS SDK publishes an API Extractor report per package listing every public entity. Matching
those names against each crate gives:

| JS package | matched | total | share |
|---|--:|--:|--:|
| app-check | 15 | 17 | 88% |
| functions | 10 | 13 | 77% |
| database | 48 | 64 | 75% |
| app | 23 | 32 | 72% |
| messaging | 10 | 15 | 67% |
| installations | 6 | 9 | 67% |
| auth | 84 | 128 | 66% |
| **firestore (lite)** | 72 | 109 | 66% |
| storage | 28 | 46 | 61% |
| analytics | 29 | 48 | 60% |
| remote-config | 16 | 30 | 53% |
| data-connect | 30 | 65 | 46% |
| firestore (full) | 81 | 183 | 44% |
| ai | 21 | 194 | 11% |
| performance | 6 | 6 | 100% |

Read these as estimates. The match is by name, and pessimistic: an API shipped under a more
idiomatic Rust name counts as missing (`getDownloadURL` is `download_url_request` here), while
`performance`'s 100% means only that its six public entities exist — its traces still do not reach
the backend. The numbers are useful for tracking movement, not for grading.

## Firestore: we are `firestore/lite`, plus listeners

This is the most important thing the comparison surfaced. The JS SDK ships **two** Firestore
clients from one package:

- `firebase/firestore/lite` — 109 public entities. One-shot `getDoc`/`getDocs`/`setDoc`,
  transactions, batches, aggregates. No `onSnapshot`, no cache, no offline.
- `firebase/firestore` — 183 public entities. The lite surface plus the sync engine: a local
  document cache (IndexedDB or memory), a mutation queue with latency compensation, a query engine
  that runs queries against the cache, watch-stream resumption, limbo-document resolution, index
  management and backfill, LRU garbage collection, multi-tab coordination, bundle loading.

The subsystems behind that second client are `local/` (17.6k lines), `core/` (12k) and `remote/`
(7k). This crate has none of them, and it is not an accident: **this crate implements the lite
client and adds `on_snapshot` on top of the real gRPC `Listen` stream.** 66% of lite, 44% of full.

That framing matters because it changes what "the gap" is. Not "our Firestore is half-finished"
but "our Firestore is a complete-ish thin client, and the offline engine is a separate product
decision". The consequences to be honest about:

- `SnapshotMetadata::has_pending_writes` is always `false` here. In the JS SDK a local write shows
  up in listeners immediately, marked pending, before the server acknowledges it. Without a
  mutation queue there is nothing to mark.
- `from_cache` is `!current` — true before the watch stream reaches `CURRENT`, false after. The JS
  SDK also raises `fromCache: true` snapshots from the local cache when the network goes away
  (`OnlineStateTracker`, after `MAX_WATCH_STREAM_FAILURES = 1` failure or a 10s timeout). With no
  cache there is nothing to raise, so a disconnected listener here simply retries.
- No `getDocFromCache`, `enableIndexedDbPersistence`, `waitForPendingWrites`, `loadBundle`,
  `onSnapshotsInSync`, or persistent index management.
- Existence filters are handled the coarse way: a filter mismatch drops the resume token and
  re-runs the target from scratch. The JS SDK first tests the server's bloom filter
  (`remote/bloom_filter.ts`) to see whether the mismatch affects documents it holds, and re-queries
  only when it must.

## What each product is missing, at the architecture level

Names in `code` are JS files, relative to `packages/`.

**auth** (66%) — the flows are here; the browser is not. Missing: the popup/redirect resolver
(`auth/src/platform_browser/popup_redirect.ts`), `RecaptchaVerifier` and reCAPTCHA Enterprise
config, the password-policy API, `beforeAuthStateChanged`, `authStateReady`,
`getAdditionalUserInfo`, `revokeAccessToken`, `updateCurrentUser`, and the persistence *variants*
(`browserLocalPersistence`, `indexedDBLocalPersistence`, …) — this crate has one pluggable
persistence trait instead, which is the better shape for Rust but does not answer to those names.

**database** (75%) — reads, writes, queries, transactions and `onDisconnect` work over the real
wire protocol. Missing: the sync engine (`database/src/core/SyncTree.ts`, `SyncPoint.ts`,
`WriteTree.ts`, `view/`), which is what lets the JS SDK serve a query from local state, apply
writes optimistically and reorder events; `.info/connected` and the server time offset;
`onChildMoved`; `off()`; `goOffline`/`goOnline`. Query listeners here re-query rather than
subscribing to a filtered view.

**storage** (61%) — high fidelity. The retry rules match exactly (`>=500`, 408, 429, plus
per-request extras; 2 minutes for operations, 10 minutes for uploads). Missing: `getDownloadURL`
under that name (the request builder exists), the `UploadTask` observer API in its JS shape, and
`getBlob`/browser-specific entry points.

**functions** (77%) — the callable protocol, including streaming and the App Check/FID/auth
headers, matches. The error taxonomy is shared with core now. Missing: little of substance.

**app-check** (88%) — the closest match of any product, and in one respect better: where the JS SDK
returns a *dummy token* carrying an error field, this crate returns a typed error that carries the
cached token. The throttling policy is ported exactly (403/404 → one day, everything else →
`calculateBackoffMillis`). Missing: reCAPTCHA v3/Enterprise attestation outside wasm, which needs a
browser.

**remote-config** (53%) — fetch, activate, ETag handling, defaults, custom signals and the minimum
fetch interval are here. Missing: the persisted throttle (`remote-config/src/client/retrying_client.ts`
stores `throttleEndTimeMillis` and `backoffCount` so a 429 survives a restart), realtime config
updates (`client/realtime_handler.ts`, an 8-retry streaming connection), and the A/B testing
(`abt/`) integration.

**analytics** (60%) — a deliberate divergence, described below.

**messaging** (67% on names, 0% in substance on native) — the JS SDK is a web-push client: service
worker registration, VAPID keys, `onMessage`/`onBackgroundMessage`, token rotation. Outside a
browser there is no push channel to attach to, so the native `get_token` **invents a token and
stores it**. That is the sharpest edge in this SDK: the call succeeds, the string looks like a
token, and a server that pushes to it reaches nobody. It should return an error on targets that
cannot receive messages; until it does, the doc comment on `Messaging::get_token` is the only
warning a caller gets.

**performance** (100% on names, ~15% in substance) — traces and network requests are recorded
correctly and never accepted by the backend, because the transport envelope is wrong. The JS SDK
posts to Firelog (`https://firebaselogging.googleapis.com/v0cc/log?format=json_proto`) with:

```json
{ "request_time_ms": "...", "client_info": { "client_type": 1, "js_client_info": {} },
  "log_source": 462,
  "log_event": [ { "source_extension_json_proto3": "<JSON-encoded PerfMetric>" } ] }
```

where each `PerfMetric` is `{ application_info, trace_metric | network_request_metric }`
(`performance/src/services/transport_service.ts`, `perf_logger.ts`). Batches are capped at 1000
events per request, flushed 40 at a time. This crate posts its own flat payload instead. That is
the whole gap: the recording layer is fine.

**ai** (11%) — the JS `ai` package has grown to 194 public entities (models, chat sessions,
streaming, function calling, response schemas, Imagen, live audio, on-device hybrid inference).
This crate has the request factory and one `generate_text` helper.

**data-connect** (46%) — queries and mutations execute; the generated-SDK surface, the cache
providers and the subscription types are missing.

## Deliberate divergences

These are not gaps; they are places where the Rust port should not follow the JS SDK.

- **The service registry is typed.** The JS container stores services as `any` and looks them up by
  string; a mismatch is invisible. Here a service declares `Service::NAME` once and is resolved by
  type, and a mismatch is a `MismatchingServiceType` error. See CONTRIBUTING.md.
- **App Check errors are typed rather than dummy tokens.** The JS SDK signals failure by handing
  back a token object whose `token` field is a placeholder and whose `error` field explains why.
  This crate returns `Result`, and the error carries the cached token when there is one.
- **Analytics uses the GA4 Measurement Protocol, not `gtag.js`.** The JS SDK's analytics is a
  wrapper around Google's browser tag: it injects a script and defers to it. There is no such thing
  to wrap outside a browser, so this crate posts events to the Measurement Protocol, which requires
  an `api_secret` and is *not* the same product. Anyone expecting `logEvent` to behave like the web
  SDK's will be surprised, which is why the coverage table says 15%.
- **Realtime Database transactions use REST compare-and-set.** The JS SDK applies the update
  optimistically to its local tree, sends it over the realtime connection and retries up to 25
  times when the server's hash disagrees. This crate reads with `X-Firebase-ETag`, writes with
  `if-match`, and retries on 412 — the same 25 attempts, the same guarantee, no optimistic local
  state. What a caller loses is the immediate local echo of the pending value.
- **One credential and transport layer.** The JS packages each build their own fetch calls; here
  `firebase_core::platform::{credentials, http}` is shared, which is what lets a product depend on
  nothing but core.
- **No compat layer.** Half the JS packages exist twice (`auth` and `auth-compat`, …) to support
  the v8 namespaced API. This SDK has one API.

## Behavioural constants

Ported from the JS SDK and worth keeping in sync. Where the value differs, the reason is given.

| Constant | JS | here | note |
|---|---|---|---|
| `calculateBackoffMillis` | 1s interval, ×2, ±50%, 4h cap | same | `util/src/exponential_backoff.ts` |
| App Check throttle after 403/404 | 1 day | same | `app-check/src/providers.ts` |
| App Check throttle, other errors | `calculateBackoffMillis` | same | |
| Installations token buffer | 1 hour | same | refresh before the token can expire mid-request |
| Installations pending-registration timeout | 10s | same | another tab's stalled registration |
| Storage operation retry budget | 2 min | same | `storage/src/implementation/constants.ts` |
| Storage upload retry budget | 10 min | same | |
| Storage retry statuses | 5xx, 408, 429 | same | |
| FCM registration retries | 3, from 5s | same | `messaging/src/util/constants.ts` |
| Auth proactive refresh margin | 5 min | same | `auth/src/core/user/proactive_refresh.ts` |
| Auth on-demand refresh margin | 30s | 5 min | this crate refreshes earlier; more calls, fewer races |
| Firestore stream backoff | 1s, ×1.5, 60s cap | 200ms, ×2, 10s cap | ours retries a dropped listen sooner and gives up waiting earlier |
| Firestore transaction attempts | 5 | 5 | |
| Database transaction retries | 25 | 25 | same count, different mechanism (hash vs ETag) |
| Database reconnect delay | 1s → 5 min, ×1.3, reset after 30s connected | none | ours reconnects on the next listen, with no backoff — a robustness gap |

## Keeping this document honest

- Every ported module cites its `packages/**` source in a doc comment. When you port a behaviour,
  cite the file you read it from.
- When you copy a constant, copy the comment explaining it too, and add a row above if it is one a
  reader would want to check.
- Re-run `scripts/api_parity.py` against a fresh checkout when updating `docs/coverage.toml`; the
  numbers here are from v12.18.0 and the JS SDK moves.
