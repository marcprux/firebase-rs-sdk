# Firebase rs SDK Unofficial

This is an unofficial Rust SDK for Firebase.

## Modules

The Firebase Rust SDK includes 14 modules, each mapping to a Firebase service:

- [ai](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/ai)
- [analytics](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/analytics)
- [app](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/app)
- [app_check](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/app_check)
- [auth](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/auth)
- [data_connect](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/data_connect)  
- [database](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/database)
- [firestore](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/firestore)
- [functions](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/functions)
- [installations](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/installations)
- [messaging](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/messaging)
- [performance](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/performance)
- [remote_config](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/remote_config)
- [storage](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/storage)

The following modules are used internally by the library and have no direct public API.

- [component](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/component)
- [logger](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/logger)
- [platform](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/platform)
- [util](https://github.com/dgasparri/firebase-rs-sdk/tree/main/src/util)

Note that this library is provided _as is_. Even the more mature modules have not been exhaustively tested. All the code published passes `cargo test`. There is an effort to port the tests of the official JavaScript SDK, but there is no guarantee that the test coverage is complete.

## Coverage

The tables below record how much of the official Firebase API surface each module covers, based on
a September 2026 audit that compared the Rust code against the modular JavaScript SDK (v9+) and, for
the mobile-relevant services, the Firebase C++ SDK. "Coverage" is the share of the public JS API for
that product that is implemented and actually reaches the real backend; the "Backend" column says
whether the module talks to the production endpoints or only to an in-memory simulation. The
percentages are estimates and deliberately stricter than the ones in each module's
`PORTING_STATUS.md`. `tests/live_endpoints.rs` verifies the "live" rows against a real project.

### Summary

| Module | Coverage | Backend | Verified live | Notes |
|---|---:|---|:---:|---|
| installations | 85% | real (Installations REST v1) | yes | FID generation, registration, token refresh, delete, id-change listeners |
| storage | 85% | real (`firebasestorage.googleapis.com/v0`) | emulator | uploads, downloads, metadata, paginated list, delete, resumable uploads with progress/pause/resume/cancel, JS error codes and `encodeURIComponent` paths |
| app | 70% | n/a | yes | app lifecycle, options, component container, heartbeat header |
| data_connect | 65% | real (`firebasedataconnect.googleapis.com/v1`) | no | executeQuery / executeMutation, emulator, subscriptions |
| auth | 75% | real (Identity Toolkit v1/v2 + securetoken) | emulator + online | email, phone, custom-token, IdP credential and MFA flows verified end to end; typed error codes; listeners; OAuth popup/redirect UI flows still delegated to the host |
| functions | 75% | real (`cloudfunctions.net` / custom domain / emulator) | emulator + online | callable protocol with auth, App Check and FID headers, streaming callables, URL callables, timeouts |
| remote_config | 50% | real (`firebaseremoteconfig.googleapis.com/v1`) | yes | fetch, ETag-based activate, defaults with correct value sources, custom signals, typed getters |
| app_check | 45% | real exchange endpoint, untested | no | custom provider and refresher only on native; reCAPTCHA is wasm-only |
| firestore | 50% | real REST for one-shot ops; realtime is simulated | emulator + online | CRUD, composite queries, snapshot cursors, batches, aggregates, optimistic transactions, serde structs; no `onSnapshot` or offline |
| database | 55% | real REST + realtime WebSocket | emulator | reads/writes/queries, server-resolved `.sv` values, compare-and-set transactions, value/child listeners over the wire protocol; query listeners re-query instead of subscribing to a filtered view |
| messaging | 0% native / 40% wasm | real on wasm only | no | native path returns placeholder tokens; no message delivery anywhere |
| performance | 15% | trace API local; upload body not accepted by backend | no | traces and metrics are recorded but never ingested |
| analytics | 15% | GA4 Measurement Protocol, not gtag | no | needs an `api_secret`; not equivalent to the JS SDK |
| ai | 10% | real `generateContent` for one helper | no | request factory is correct; model, chat, streaming and Imagen missing |

### app

| JS API | Status |
|---|---|
| `initializeApp`, `getApp`, `getApps`, `deleteApp` | implemented |
| `initializeServerApp`, `registerVersion`, `onLog`, `setLogLevel`, `SDK_VERSION` | implemented |
| Component container (`_registerComponent`, `_getProvider`, `_removeServiceInstance`) | implemented (crate-private) |
| Heartbeat (`X-Firebase-Client` header) | partial, in-memory storage only, attached by App Check and Functions only |

### auth

| JS API | Status |
|---|---|
| `signInWithEmailAndPassword`, `createUserWithEmailAndPassword`, `signInAnonymously`, `signInWithCustomToken` | implemented |
| `signInWithEmailLink`, `isSignInWithEmailLink`, `sendSignInLinkToEmail` | implemented |
| `sendPasswordResetEmail`, `confirmPasswordReset`, `verifyPasswordResetCode`, `applyActionCode`, `checkActionCode`, `sendEmailVerification` | implemented; round-tripped through the emulator's out-of-band codes |
| `updateProfile`, `updateEmail`, `updatePassword`, `deleteUser`, `unlink` | implemented |
| `reauthenticateWithCredential`, `linkWithCredential` (password, OAuth, phone) | implemented |
| `signInWithPhoneNumber`, `linkWithPhoneNumber`, `reauthenticateWithPhoneNumber` | implemented (needs caller-supplied verifier); verified against the Auth emulator's SMS codes |
| Multi-factor: phone, TOTP, passkey enrol / unenrol / resolver, `getMultiFactorResolver` (`Auth::multi_factor_resolver`) | implemented; phone enrolment, challenge and unenrol verified against the emulator (endpoints now correctly target `v2`) |
| `getIdToken` (`User::get_id_token(force_refresh)` and `Auth::get_token`), `getIdTokenResult` (decoded claims, `sign_in_provider`, `sign_in_second_factor`), token refresh through `securetoken.googleapis.com` | implemented; custom claims verified via unsigned custom tokens on the emulator |
| `signInWithPopup`, `signInWithRedirect`, `linkWithPopup`, `getRedirectResult` | partial, delegates to a caller-supplied handler; no built-in flow |
| `onAuthStateChanged` | implemented; emits `Some(user)` / `None`, primes with the current state, fires only when the uid changes, unsubscribe removes the observer |
| `setPersistence` | partial, constructor-time only |
| Typed error codes (`auth/wrong-password`, `auth/user-not-found`, `auth/too-many-requests`, ...) | implemented; `AuthError::Server` carries an `AuthErrorCode` mapped with the JS `SERVER_ERROR_MAP`, unmapped codes are normalised like the JS SDK (`auth/configuration-not-found`) |
| `reload` (email verification state, `metadata`, `providerData`, second factors), `onIdTokenChanged` | implemented |
| `updateCurrentUser`, `beforeAuthStateChanged` | missing |
| `fetchSignInMethodsForEmail`, `verifyBeforeUpdateEmail`, `linkWithCredential` for email/password (anonymous upgrade), `GoogleAuthProvider.credential` / `oauth_credential` | implemented |
| `revokeAccessToken`, `validatePassword`, `updatePhoneNumber` | missing |
| `connectAuthEmulator` | implemented (`Auth::connect_emulator` / `connect_auth_emulator`), verified against the Auth emulator |
| `useDeviceLanguage`, `tenantId`, `RecaptchaVerifier`, `SAMLAuthProvider` | missing |

### firestore

| JS API | Status |
|---|---|
| `getFirestore`, `collection`, `doc`, `collectionGroup` | implemented |
| `getDoc`, `getDocs`, `setDoc` (merge, mergeFields), `updateDoc`, `deleteDoc`, `addDoc` | implemented over REST |
| `where` (all operators), `orderBy`, `limit`, `limitToLast`, `startAt`/`startAfter`/`endAt`/`endBefore` (values or document snapshots) | implemented; implicit `orderBy` for inequality fields and `__name__` matches the JS SDK |
| `serverTimestamp`, `increment`, `arrayUnion`, `arrayRemove`, `deleteField` | implemented |
| `writeBatch`, `getCountFromServer`, `getAggregateFromServer` (`count`, `sum`, `average`) | implemented; `WriteBatch::commit_with_results` returns per-write `updateTime` and `commitTime` |
| `Timestamp`, `GeoPoint`, `FieldPath`, `DocumentReference`, `CollectionReference`, `Query`, snapshots, data converters | implemented |
| Auth / App Check headers, emulator host | implemented |
| `connectFirestoreEmulator` (`FIRESTORE_EMULATOR_HOST` / builder), `documentId` | partial |
| `FieldPath` quoting (fields containing `.` or backticks are addressed literally via `FieldPath::new`) | implemented |
| `onSnapshot`, `onSnapshotsInSync` | missing, the internal sync engine is not reachable and its transport is simulated |
| `runTransaction` / `Transaction` (`get`, `get_all`, `set`, `update`, `delete`, converters) | implemented like the JS SDK: optimistic, `updateTime` preconditions, `verify` entries for read-only documents, retry on `failed-precondition` / `aborted` (5 attempts) |
| Write preconditions (`Precondition`, `ConditionalWrite`) | implemented for transactions; not yet exposed on `WriteBatch` |
| `and`, `or` composite filters (`where_filter`, `and`, `or`), `deleteField` (`FirestoreValue::delete_field`) | implemented, verified against the emulator |
| `vector` / `VectorValue` | missing |
| `getDocFromCache`, `getDocFromServer`, `enableNetwork`, `disableNetwork`, `waitForPendingWrites`, `terminate` | missing |
| Offline persistence (`persistentLocalCache`, `enableIndexedDbPersistence`), bundles, named queries, index configuration | missing |
| Serde integration (`set_doc_as`, `get_doc_as`, `get_docs_as`, `SerdeConverter`, `Transaction::get_as/set_as`, `WriteBatch::set_as/update_as`) | implemented; `Timestamp`, `GeoPoint`, `BytesValue` and `FirestoreValue` sentinels map to native Firestore types |

### database (Realtime Database)

| JS API | Status |
|---|---|
| `getDatabase`, `ref`, `child`, `parent`, `root`, `key`, `push` | implemented |
| `set`, `update`, `remove`, `setPriority`, `setWithPriority` | implemented over REST; a write no longer reads the whole database first |
| `connectDatabaseEmulator` | implemented; moves the REST channel and the realtime connection together, verified against the Database emulator |
| `onDisconnect().set/setWithPriority/update/remove/cancel` | implemented over WebSocket; `.sv` values are left for the server to resolve at disconnect time |
| `query`, `orderByChild/Key/Value/Priority`, `startAt/After`, `endAt/Before`, `equalTo`, `limitToFirst/Last` | implemented for `get`, verified against the emulator (missing `.indexOn` surfaces the server's error) |
| `get` | implemented; served from a listener's live view when one is attached, otherwise from the server |
| `onValue`, `onChildAdded`, `onChildChanged`, `onChildRemoved` | implemented over the realtime protocol (`q` / `n` frames); remote writes reach listeners, denied listens report `database/permission-denied` |
| Query listeners (`onValue(query, ...)`) | partial: the client listens to the path and re-runs the query on change; no tagged server-side filtered views yet |
| `serverTimestamp`, `increment` | implemented; resolved by the server, so concurrent increments cannot lose updates |
| `runTransaction` | implemented as compare-and-set over the REST ETag / `if-match` protocol, retrying on conflict (25 attempts) |
| `goOnline`, `goOffline` | partial |
| `onChildMoved`, `off`, `enableLogging`, `refFromURL`, `DataSnapshot.forEach/exportVal` | missing |
| Keepalive, reconnect and re-listen, control frames, multi-frame messages on the WebSocket | missing |

### storage

| JS API | Status |
|---|---|
| `getStorage`, `ref` (path, `gs://`, `https://`), `connectStorageEmulator` | implemented |
| `uploadBytes`, `uploadString` | implemented |
| `getDownloadURL`, `getBytes`, `getStream` (native), `getBlob` (wasm) | implemented |
| `getMetadata`, `updateMetadata`, `list`, `listAll`, `deleteObject` | implemented; `list` paginates with `max_results` (1..=1000) and page tokens, verified against the Storage emulator |
| `uploadBytesResumable`, `UploadTask.pause/resume/cancel`, `UploadTaskSnapshot`, `on('state_changed')` | implemented; chunked progress via `UploadTaskHandle::on_state_changed`, cancellation discards the resumable session server-side and reports `storage/canceled` |
| `UploadTask` as a promise (`then`/`catch`), `TaskEvent`/`StorageObserver` object form, resuming a session after a process restart | missing; drive the task with `run_to_completion` and closures instead |
| Error codes (`object-not-found`, `bucket-not-found`, `unauthenticated`, `unauthorized`, `unauthorized-app`, `quota-exceeded`, `retry-limit-exceeded`, `unknown` with status and body) | implemented per the JS `sharedErrorHandler` / `objectErrorHandler`; verified against the Storage emulator |

### functions

| JS API | Status |
|---|---|
| `getFunctions` (region or custom domain), `httpsCallable` | implemented |
| Callable protocol: `data` / `result` envelope, gRPC status error mapping | implemented |
| `Authorization`, `Firebase-Instance-ID-Token`, `X-Firebase-AppCheck`, `X-Firebase-Client` headers | implemented |
| `httpsCallable(...).stream()` (server-sent events) | implemented natively (`stream_async` -> `CallableStream`), verified against the Functions emulator |
| `connectFunctionsEmulator` | implemented, verified against the Functions emulator |
| `httpsCallableFromURL`, `HttpsCallableOptions` (timeout, limited-use App Check) | implemented |
| `@type` `Int64Value` / `UInt64Value` decoding | missing |

### remote_config

| JS API | Status |
|---|---|
| `getRemoteConfig`, `fetchConfig`, `activate`, `fetchAndActivate`, `ensureInitialized` | implemented; `activate` applies the JS ETag rules (no template, no ETag or unchanged ETag activates nothing) |
| `getAll`, `getBoolean`, `getNumber`, `getString`, `getValue`, `Value.getSource` | implemented; defaults report `default`, activated parameters `remote`, unknown keys `static` |
| `defaultConfig`, `settings` (fetch timeout, minimum interval), `fetchTimeMillis`, `lastFetchStatus` | implemented |
| `setCustomSignals` | implemented |
| ETag / `If-None-Match`, `NO_CHANGE`, `NO_TEMPLATE`, `EMPTY_CONFIG` states | implemented |
| Persistent storage | partial, in-memory by default; file storage is opt-in |
| `onConfigUpdate` (real-time), `isSupported`, `setLogLevel`, `Retry-After` throttling | missing |

### installations

| JS API | Status |
|---|---|
| `getInstallations`, `getId`, `getToken`, `deleteInstallations`, `onIdChange` | implemented |
| FID generation, `FIS_v2` registration, `authTokens:generate`, re-registration on 401 / 404 | implemented |
| File-backed persistence (native), IndexedDB (wasm) | implemented |

### app_check

| JS API | Status |
|---|---|
| `initializeAppCheck`, `getToken`, `getLimitedUseToken`, `onTokenChanged`, `setTokenAutoRefreshEnabled` | implemented |
| `CustomProvider` | implemented |
| `ReCaptchaV3Provider`, `ReCaptchaEnterpriseProvider` | wasm-only |
| Debug token provider (`exchangeDebugToken`), native persistence | missing |
| C++ equivalents (`DeviceCheck`, `AppAttest`, `PlayIntegrity`) | missing |

### data_connect

| JS API | Status |
|---|---|
| `getDataConnect`, `connectDataConnectEmulator` | implemented |
| `queryRef`, `executeQuery`, `subscribe`, `mutationRef`, `executeMutation`, `toQueryRef`, `SerializedRef` | implemented |
| `terminate`, `validateArgs`, `setLogLevel` | missing |

### messaging

| JS API | Status |
|---|---|
| `getToken`, `deleteToken` (FCM registrations API) | wasm-only; native returns a locally generated placeholder |
| `onMessage`, `onBackgroundMessage` | stub, handlers are stored but never invoked |
| `isSupported`, service-worker integration | wasm-only |

### performance

| JS API | Status |
|---|---|
| `initializePerformance`, `trace`, `start`, `stop`, `record`, `putMetric`, `incrementMetric`, attributes, instrumentation flags | implemented locally |
| Upload to `firebaselogging.googleapis.com` | not accepted, the request body does not match the Firelog schema |
| Remote settings (`fireperf:fetch`), sampling | missing |

### analytics

| JS API | Status |
|---|---|
| `getAnalytics`, `logEvent`, `setDefaultEventParameters`, `setAnalyticsCollectionEnabled` | implemented via GA4 Measurement Protocol (requires `api_secret`) |
| `setUserId`, `setUserProperties`, `getGoogleAnalyticsClientId`, `isSupported`, `initializeAnalytics` | missing |
| `setConsent`, `settings` | stub |

### ai

| JS API | Status |
|---|---|
| `getAI`, backend selection (`GoogleAIBackend`, `VertexAIBackend`), error taxonomy, request factory | implemented |
| `generateContent` (single-prompt helper) | implemented |
| `GenerativeModel.generateContent` / `generateContentStream`, `startChat`, `countTokens` | missing |
| `ImagenModel`, `LiveGenerativeModel`, `Schema` builders, response helpers | missing |

## Feature Flags

This library is WASM compatible and offers the following cargo features:

- `wasm-web`: enables the bindings required to compile the crate for `wasm32-unknown-unknown` (e.g. `wasm-bindgen`, `web-sys`, `gloo-timers`). Activate this when you target the web or run wasm-specific tests.
- `experimental-indexed-db`: turns on IndexedDB-backed persistence for modules that support it (currently App Check). Without this flag, wasm builds fall back to in-memory storage while keeping the same API.

To enable those features in `Cargo.toml`:

```toml
[dependencies]
firebase-rs-sdk = { version = "X.XX", features = ["wasm-web", "experimental-indexed-db"] }
```

To build or test with these features

```bash
cargo check --target wasm32-unknown-unknown --features wasm-web,experimental-indexed-db
cargo test --target wasm32-unknown-unknown --features wasm-web,experimental-indexed-db
cargo build --target wasm32-unknown-unknown --features wasm-web,experimental-indexed-db
```

If you only need the wasm bindings and not IndexedDB persistence, omit `experimental-indexed-db` from the list.

## Connection to Google's official JavaScript SDK

Because Firebase APIs are not fully documented, we relied heavily on the official JavaScript SDK (and, to a lesser extent on the C++ SDK) to implement the functions. The JavaScript SDK appears as one of the most complete in terms of functions and calls to the service, and it is one of the few that implements the services from scratch without depending on external Java libraries. Moreover, it offers one of the most complete and well-documented APIs.

There are often direct and clear parallels between the TypeScript methods of the JS SDK (initializeApp(), getFirestore(), collection(), getDocs()) and their counterparts in this Rust SDK (initialize_app(), get_firestore(), collection(), get_docs()).

For that reason, sometimes for calls and features it might be useful to refer directly to the JavaScript SDK:

- Quickstart Guide: <https://firebase.google.com/docs/web/setup>
- API references: <https://firebase.google.com/docs/reference/js/>
- SDK Github repo: <https://github.com/firebase/firebase-js-sdk>

(These resources are maintained by Google and the community.)

If you want to contribute, donating your time and AI resources is the most valuable way to support this project. See the [`CONTRIBUTING.md`](https://github.com/dgasparri/firebase-rs-sdk/blob/main/CONTRIBUTING.md) page on how to help.

## Example

Connects to the Firestore service, populates an in-memory-only document with some mock values, and retrieves them.

```rust,no_run
use std::collections::BTreeMap;
use std::error::Error;

use firebase_rs_sdk::app::{initialize_app, FirebaseAppSettings, FirebaseOptions};
use firebase_rs_sdk::firestore::*;

# #[cfg(target_arch = "wasm32")]
# fn main() {}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let firebase_config = FirebaseOptions {
        api_key: Some("demo-api-key".into()),
        project_id: Some("demo-project".into()),
        ..Default::default()
    };

    let app = initialize_app(firebase_config, Some(FirebaseAppSettings::default())).await?;
    let firestore_arc = get_firestore(Some(app.clone())).await?;
    let firestore = Firestore::from_arc(firestore_arc);

    // Talk to the hosted Firestore REST API. Configure credentials/tokens as needed.
    let client = FirestoreClient::with_http_datastore(firestore.clone())?;

    let cities = load_cities(&firestore, &client).await?;

    println!("Loaded {} cities from Firestore:", cities.len());
    for city in cities {
        let name = field_as_string(&city, "name").unwrap_or_else(|| "Unknown".into());
        let state = field_as_string(&city, "state").unwrap_or_else(|| "Unknown".into());
        let country = field_as_string(&city, "country").unwrap_or_else(|| "Unknown".into());
        let population = field_as_i64(&city, "population").unwrap_or_default();
        println!("- {name}, {state} ({country}) — population {population}");
    }

    Ok(())
}

/// Mirrors the `getCities` helper in `JSEXAMPLE.ts`, issuing the equivalent modular query
/// against the remote Firestore backend.
async fn load_cities(
    firestore: &Firestore,
    client: &FirestoreClient,
) -> FirestoreResult<Vec<BTreeMap<String, FirestoreValue>>> {
    // The modular JS quickstart queries the `cities` collection.
    let query = firestore.collection("cities")?.query();
    let snapshot = client.get_docs(&query).await?;

    let mut documents = Vec::new();
    for doc in snapshot.documents() {
        if let Some(data) = doc.data() {
            documents.push(data.clone());
        }
    }

    Ok(documents)
}

fn field_as_string(data: &BTreeMap<String, FirestoreValue>, field: &str) -> Option<String> {
    data.get(field).and_then(|value| match value.kind() {
        ValueKind::String(text) => Some(text.clone()),
        _ => None,
    })
}

fn field_as_i64(data: &BTreeMap<String, FirestoreValue>, field: &str) -> Option<i64> {
    data.get(field).and_then(|value| match value.kind() {
        ValueKind::Integer(value) => Some(*value),
        _ => None,
    })
}
```

For further details, refer to the example [`./examples/firestore_select_documents.rs`](https://github.com/dgasparri/firebase-rs-sdk/blob/main/examples/firestore_select_documents.rs) or run `cargo run --example firestore_select_documents`.

## Live endpoint tests

Besides the offline unit tests, `tests/live_endpoints.rs` exercises real backends: Auth, Firestore,
Realtime Database, Storage and Functions against the Firebase Local Emulator Suite (no credentials
needed), and
Installations and Remote Config against a real project when credentials are configured. See
[`CONTRIBUTING.md`](https://github.com/dgasparri/firebase-rs-sdk/blob/main/CONTRIBUTING.md#live-endpoint-tests).

```bash
npm install -g firebase-tools && npm ci --prefix firebase-emulator/functions   # once
scripts/emulator_test.sh
```

## Copyright

This library is licensed under the Apache License, Version 2.0.

This library is distributed ‘as is’ without warranties or conditions of any kind.

## How to contribute

The porting process is time- and AI-intensive; any help is appreciated. See [`CONTRIBUTING.md`](https://github.com/dgasparri/firebase-rs-sdk/blob/main/CONTRIBUTING.md) for details.
