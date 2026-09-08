## Porting status

- app 80% `[########  ]`

_As of October 24th, 2025_

- The async app lifecycle (`initialize_app`, `get_app`, `get_apps`, `delete_app`, version registration, logging)
  mirrors `packages/app/src/api.ts`, including hashed naming for server apps and duplicate-config detection.
- Environment helpers now expose `get_default_app_config`, `is_browser`, and `is_web_worker`, sourcing configuration
  from `__FIREBASE_DEFAULTS__`, `FIREBASE_CONFIG`, and related overrides so the Rust port can auto-bootstrap like the
  JS SDK on both native and WASM targets.
- Server-side apps support `release_on_deref` semantics via deferred deletion, matching the JS `FirebaseServerApp`'s
  FinalizationRegistry behaviour while continuing to validate auth/app-check tokens.
- Internal component utilities (`add_component`, `add_or_overwrite_component`, `get_provider`, `remove_service_instance`,
  `clear_components`, `register_component`) are re-exported through `app::registry`, mirroring
  `packages/app/src/internal.ts` for downstream modules.

## 2026-09-08 update: component resolution is no longer racy

`cargo test` failed roughly one run in four, usually with a burst of unrelated tests reporting
"Failed to obtain Firestore instance" or "Auth service not initialized". Three separate causes:

- **`ComponentContainer::get_provider` was a check-then-insert race.** It released the lock between
  the lookup and the insert, so two threads each built a provider and the second overwrote the
  first — discarding the component that had just been set on it, and leaving every later lookup
  with an empty provider. Lookup and insert now happen under one lock, and
  `add_or_overwrite_component` installs the replacement under the same lock.
  Regression test: `component::tests::concurrent_lookups_never_discard_a_configured_provider`
  (fails reliably without the fix).
- **Service registration was one-shot.** Every module registered its component inside a
  `LazyLock<()>`, so once fired it never ran again; a registry that had lost the component (or an
  app created before the module was first used) stayed broken. The components are still built once,
  but `register_component` — which is idempotent and propagates to existing apps — now runs on
  every `register_*_component()` call.
- **Registration could not reach an app the registry does not know about.** Propagation walks the
  global app map, which excludes apps built directly (as unit tests do) or removed concurrently.
  `get_firestore` and `auth_for_app` now attach the component to the app's own container when it is
  missing. Regression tests: `an_app_outside_the_registry_still_resolves_firestore` /
  `..._resolves_auth`.

The app tests' `reset()` helper also wiped the process-global component registry before every
serialized test, which is what turned a narrow race into a cascade; it now clears only the app map,
versions and heartbeat store, and the `clear_components` test restores the registry under the same
guard it clears it with, so no other test ever observes an empty registry.

Measured after the fix: 12 consecutive full `cargo test --lib` runs with no failures (previously
about one run in four failed, once taking 31 tests down at a time).

## Development status as of 24th October 2025

- Core functionalities: Mostly implemented 
- Tests: 19 unit tests (passing)
- Documentation: Most public functions are documented
- Examples: None provided

DISCLAIMER: This is not an official Firebase product, nor it is guaranteed that it has no bugs or that it will work as intended.


## What’s Implemented

- **Core async API surface** (`api.rs` / `namespace.rs`)
  - `initialize_app`, `get_app`, `get_apps`, `delete_app`, and server-app entry points now expose async-first signatures that mirror the Firebase JS naming.
  - Basic namespace exposure so other services can obtain the default app instance.
- **Component infrastructure** (`component.rs`, `registry.rs`)
  - Component container/provider abstractions and global registration logic that mirror the JS `ComponentContainer`.
  - Public `register_component` hook used by other ports (Auth, Storage, Firestore).
- **Error handling & constants** (`errors.rs`, `constants.rs`)
  - Ported error enums/messages covering duplicate apps, invalid names, missing options, etc.
  - Default entry name and platform logging constants.
- **Logging** (`logger.rs`)
  - Lightweight logger abstraction with global log level, mirroring the JS logging hooks.
- **Types** (`types.rs`)
  - `FirebaseApp`, `FirebaseAppSettings`, `FirebaseOptions`, and server-app scaffolding with option/config comparisons.
- **Private/internal exports** (`private.rs`)
  - Bridges for internal modules to reach app types without exposing them publicly, alongside helpers for component
    registration and provider management.
- **Environment helpers** (`types.rs`, `platform/environment.rs`)
  - `get_default_app_config`, `is_browser`, and `is_web_worker` mirror the JS detection logic, respecting
    `__FIREBASE_DEFAULTS__`, `FIREBASE_CONFIG`, and forced environment overrides for both native and WASM builds.
- **Server app lifecycle** (`api.rs`, `types.rs`)
  - Server apps honour `release_on_deref` by spawning deferred teardown through the registry, while retaining the
    hashed naming, reference counting, and token TTL validation implemented in JS.
- **Internal API surface** (`private.rs`)
  - `_addComponent`, `_addOrOverwriteComponent`, `_clearComponents`, `_getProvider`, `_removeServiceInstance`, and
    `_registerComponent` equivalents are exposed so other modules can mirror the JS internal API without duplicating
    registry logic.
- **Heartbeat service** (`heartbeat.rs`, `platform/browser/indexed_db.rs`)
  - Heartbeats persist to IndexedDB on WASM targets (with `wasm-web` + `experimental-indexed-db`) and fall back to an
    in-memory store elsewhere, while building the v2 heartbeat header payload consumers attach to outgoing requests.

These pieces are enough for other Rust modules (Auth, Storage, Firestore) to register components and resolve `FirebaseApp`
instances.

## Gaps vs `packages/app`

The TypeScript module still includes functionality that is only partially or not yet ported:

1. **Platform logger enrichment**
   - `PlatformLoggerServiceImpl` currently concatenates version components but omits the richer SDK metadata and user
     agent strings produced in `platformLoggerService.ts`.
2. **Browser auto-configuration**
   - While environment detection is in place, parsing config from script tags/cookies, measurement ID normalization,
     and whitespace trimming from `getDefaultAppConfig` remain unported.
3. **Server app advanced APIs**
   - `initialize_server_app` still lacks the overload that accepts an existing `FirebaseApp`, the `toJSON` stub, and
     finer-grained release controls covered in `firebaseServerApp.ts`.
4. **Testing parity**
   - The JS suite includes extensive coverage (heartbeat, platform logger, indexedDB failure modes) that still needs
     to be reflected in Rust unit and integration tests.

## Next Steps - Detailed Completion Plan

1. **Platform logger enrichment**
   - Extend `PlatformLoggerServiceImpl` to build the Firebase user-agent string (bundling SDK identifiers, platform
     hints, and variants) and ensure eager registration mirrors `registerCoreComponents.ts`.

2. **Browser auto-bootstrap**
   - Parse config from script tags and cookies, normalize measurement IDs, and port whitespace/undefined guards so
     `get_default_app_config` matches the JS heuristics.

3. **Server app API parity**
   - Add the overload that accepts an existing `FirebaseApp`, port the `toJSON` behaviour, and expose ergonomic
     wrappers for explicit release so server environments behave exactly like `firebaseServerApp.ts`.

4. **Testing & documentation**
   - Port the remaining JS tests (platform logger/server app/browser auto-config) and expand module docs/examples to
     cover heartbeat header consumption and browser persistence behaviour.

### Recent Progress
- Hooked `get_default_app_config`, `is_browser`, and `is_web_worker` into the build so native and WASM targets reuse
  environment-driven defaults instead of stubs.
- Added release-on-drop semantics for `FirebaseServerApp`, updated `delete_app` to manage server refs, and verified the
  behaviour with async tests.
- Re-exported the JS internal component helpers (`_addComponent`, `_getProvider`, etc.) through `app::private` and
  added coverage to exercise registry propagation and service eviction.
- Expanded unit tests for server apps and private helpers to guard future regressions.

Completing these items will bring the Rust app module to parity with `@firebase/app` and ensure downstream services behave
consistently across ports.
