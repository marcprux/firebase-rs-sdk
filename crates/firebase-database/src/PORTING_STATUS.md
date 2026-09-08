## Porting status

Coverage: see the [table in the repository README](https://github.com/marcprux/firebase-rs-sdk#coverage), generated from `docs/coverage.toml`. The notes below are the porting history and are not kept in sync with that table.

==As of October 21th, 2025== 

Prompt: Compare the original JS/Typescript files in ./packages/database and the ported files in Rust in ./src/database, and give me an estimated guess, in percentage, of how much of the features/code of the Firebase JS SDK has been ported to Rust for this module

I'll put together a clear bullet list showing about 30% of the JavaScript Realtime Database features appear ported to Rust, backing it with code references for implemented and missing parts, highlighting key TypeScript modules not yet ported, and finishing with suggestions like verifying features or updating documentation.

Port Estimate

  - Roughly 30 % of the packages/database TypeScript surface is reflected in Rust; the port covers basic Database/
  Reference CRUD, query builders, server values, and token-aware REST + in-memory backends.
  - Implemented pieces include the Database component wiring and backend selection (src/database/api.rs:1499-1522, src/
  database/backend.rs:15-205), core reference/query operations with listener dispatch (src/database/api.rs:773-960,
  src/database/api.rs:1078-1250), snapshot helpers (src/database/api.rs:140-210), push-ID generation, and server value
  helpers (src/database/server_value.rs:1-18), giving functional parity for simple reads/writes and REST filtering.
  - Large swaths of the JS SDK remain unported: OnDisconnect actions error out (src/database/on_disconnect.rs:20-
  50), transactions are stubbed (src/database/api.rs:905-913), and all realtime transports/Repo orchestration are
  placeholders (src/database/realtime.rs:3-56), so anything depending on core/Repo.ts, PersistentConnection.ts, or
  Transaction.ts is missing.
  - Listener support is limited to local diffing; there is no remote event streaming (repo/SyncTree logic from packages/
  database/src/core) or child-moved/query child events, nor off/once/isEqual/toString parity for QueryImpl/ReferenceImpl
  (see packages/database/src/api/Reference_impl.ts for the unported surface).
  - Auxiliary controls such as goOnline/goOffline, emulator toggles, logging, connection stats, validation utilities,
  and Node-specific entry points (packages/database/src/api/Database.ts, core/util) are absent, indicating the broader
  environment/runtime features are still outstanding.

Next steps you might consider:

  1. Prioritise porting the Repo/persistent connection stack to unlock realtime listeners, transactions, and onDisconnect semantics.
  2. Flesh out the remaining public API surface (query equality/serialization helpers, off/once, refFromURL, logging/ emulator controls) and update src/database/README.md once significant pieces land.


## Development status as of 14th October 2025

- Core functionalities: Mostly implemented
- Testing: 30 tests (passed)
- Documentation: Most public functions are documented
- Examples: 2 examples

DISCLAIMER: This is not an official Firebase product, nor it is guaranteed that it has no bugs or that it will work as intended.


## 2026-09-07 update (verified against the Realtime Database emulator)

The realtime and write paths were reworked and are now covered by `database_*` tests in
`tests/live_endpoints.rs`, run by `scripts/emulator_test.sh` against the Database emulator:

- **Wire protocol fixed.** Listens went out as `listen`/`unlisten`; the protocol uses `q` and `n`
  (`packages/database/src/core/PersistentConnection.ts`). The websocket URL also dropped the port,
  so every non-443 host (i.e. every emulator) failed to connect. Listeners now receive remote
  writes, and a listen the rules reject is reported to the caller as `database/permission-denied`
  instead of hanging.
- **Writes no longer read the whole database.** `set`/`update`/`remove` used to `GET /` before
  every write to diff listeners locally; that is O(database) per write and fails outright under
  rules that do not grant read access at the root. The client now keeps a partial mirror of only
  the paths it listens to, seeded per listener path.
- **Server values are resolved by the server.** `serverTimestamp()` and `increment()` are sent as
  `.sv` placeholders, and the write's response carries what the server stored (no `print=silent`
  in that case), so concurrent increments cannot lose updates.
- **`get()` is honest.** It was served from a whole-root cache that could be arbitrarily stale;
  it now reads from the server unless a listener keeps that location in sync, matching
  `repoGetValue`'s use of the sync tree.
- **Transactions do compare-and-set.** `run_transaction` reads with `X-Firebase-ETag`, writes with
  `if-match`, and retries the closure against fresh data on 412, up to 25 attempts.
- **`connect_database_emulator`** re-points both the REST channel and the realtime connection of an
  existing `Database`, deriving the `<project>-default-rtdb` namespace like the JS SDK.
- **Query listeners** attach to their path and re-run the query on change. A filtered listen must
  carry a tag on the wire; sending one without a tag makes the server drop the connection, so
  tagged views (`SyncTree`'s `View` bookkeeping) are the next piece of work.

## Current State

- Database component registration via `register_database_component` so `get_database` resolves out of the shared `FirebaseApp` registry.
- Async-first surfaces for `get_database`, `DatabaseReference` CRUD helpers, query listeners, and modular helpers so consumers `await` instead of relying on `block_on` shims.
- Core reference operations (`reference`, `child`, `set`, `update`, `remove`, `get`) that work against any backend and emit `database/invalid-argument` errors for unsupported paths.
- Auto-ID child creation via `DatabaseReference::push()` / `push_with_value()` and the modular `push()` helper, mirroring the JS SDK's append semantics.
- Priority-aware writes through `DatabaseReference::set_with_priority()` / `set_priority()` (and modular helpers), persisting `.value`/`.priority` metadata compatible with REST `format=export`.
- Server value helpers (`server_timestamp`, `increment`). Since 2026-09-07 the placeholders travel to the server, which resolves them; only the in-memory backend still resolves them locally.
- Child event listeners (`on_child_added`, `on_child_changed`, `on_child_removed`) with in-memory diffing and snapshot traversal utilities for callback parity with the JS SDK.
- Hierarchical navigation APIs (`DatabaseReference::parent/root`) and snapshot helpers (`child`, `has_child`, `has_children`, `size`, `to_json`) that mirror the JS `DataSnapshot` traversal utilities.
- Query builder helpers (`query`, `order_by_*`, `start_*`, `end_*`, `limit_*`, `equal_to*`) with `DatabaseQuery::get()` and REST parameter serialisation.
- `on_value` listeners for references and queries that deliver an initial snapshot and replay callbacks after local writes, returning `ListenerRegistration` handles for manual detach.
- Backend selection that defaults to an in-memory store and upgrades to a REST backend (PUT/PATCH/DELETE/GET over the shared HTTP client) including base query propagation plus optional Auth/App Check token injection.
- Unit tests covering in-memory semantics, validation edge cases, and REST request wiring through `httpmock`.
- Preliminary realtime hooks (`Database::go_online`/`go_offline`) backed by a platform-aware transport selector. The Rust port now normalises listen specs, reference-counts active listeners, and—on native targets—establishes an async WebSocket session using `tokio-tungstenite`, forwarding auth/App Check tokens and queuing listen/unlisten envelopes until the full persistent connection protocol is ported. Streaming payload handling is still pending.
- WASM builds mirror the native realtime selector: the runtime first attempts a `web_sys::WebSocket` connection and automatically falls back to an HTTP long-poll loop when sockets are unavailable, keeping `on_value` listeners alive across restrictive environments.
- `OnDisconnect` scheduling (`set`, `set_with_priority`, `update`, `remove`, `cancel`) forwards to the realtime transport when a WebSocket is available, resolving server timestamp/increment placeholders before dispatch. Under the long-poll fallback, the operations are queued and executed when the client calls `go_offline()`, providing a graceful degradation when WebSockets are unavailable.
- `run_transaction` mirrors the JS API, returning a `TransactionResult` with `committed`/`snapshot` fields. Since 2026-09-07 it is a real compare-and-set over the REST ETag / `if-match` protocol with retries, so simultaneous writers no longer overwrite each other.

### WASM Notes

- The module compiles on wasm targets when the `wasm-web` feature is enabled. Web builds attempt to establish a realtime WebSocket and transparently degrade to a long-poll fetch loop when sockets cannot open, reusing the same listener bookkeeping as native builds. `OnDisconnect` operations require an active WebSocket; on the long-poll fallback they are queued locally and executed when `go_offline()` runs, which does not fully replicate server-side disconnect handling.
- Calling `get_database(None)` is not supported on wasm because the default app lookup is asynchronous. Pass an explicit `FirebaseApp` instance instead.
- `go_online`/`go_offline` are currently stubs on wasm (and native) but provide the async surface needed for upcoming realtime work.


## Next Steps

- Tagged query views so a filtered listen is served by the server instead of a re-query per change (`core/SyncTree.ts`, `core/view/View.ts`).
- Child event parity: `on_child_moved`, query-level child listeners, and server-ordered `prev_name` semantics from `core/SyncTree.ts`.
- Connection hardening: keepalive frames, reconnect with re-listen, multi-frame messages, and the server's `t: "c"` control frames (`PersistentConnection.ts`).
- Long-poll `OnDisconnect` execution and offline queue handling on the wasm fallback (`OnDisconnect.ts`).
- `goOnline`/`goOffline`, `enableLogging`, `refFromURL` and `DataSnapshot.forEach`/`exportVal` from `Database.ts` and `Reference_impl.ts`.

### Immediate Porting Focus

1. **Tagged query views** – Assign a tag per filtered listen, send it with the `q` frame, and route the server's tagged pushes into the matching view. Verified behaviour: an untagged filtered listen answers `internal_error` and the server then closes the connection; a tagged one returns exactly the query window and pushes only its changes.
2. **Child listener parity** – Port the remaining event registrations (`onChildMoved`, query-level child listeners, cancellation hooks) from `Reference_impl.ts` and `SyncTree.ts`.
3. **Connection lifecycle** – Keepalive, reconnect with re-listen, and the control frames (`t: "c"`) the server sends, mirroring `PersistentConnection.ts`.
