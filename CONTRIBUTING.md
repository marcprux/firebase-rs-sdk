# Contributing to the unofficial Firebase RS SDK

Any contribution to port additional features and create new tests is greatly appreciated. Because a significant portion of the code is AI generated, extra eyes on testing and correctness are especially valuable.

## Setting up the environment

To set up the development environment, first clone the GitHub repository:

> git clone <https://github.com/marcprux/firebase-rs-sdk.git>

Cloning the Firebase JavaScript SDK repository is optional but helpful for reference:

> <https://github.com/firebase/firebase-js-sdk.git>

You may also want to copy all the files and subfolders from the JS SDK `./packages` folder into this repo’s `./packages` folder for easier reference. These files also help the AI analyze JS SDK features. Under Windows Command Line:

> XCOPY /E firebase-js-sdk\packages\* firebase-rs-sdk\packages\.

Copy the doc files in the `docs-devsite` folder as well. Those files contain documentation for the API calls. Under Windows Command Line:

> COPY firebase-js-sdk\docs-devsite\* firebase-rs-sdk\docs-devsite\.

### Setting up the testing environment

Testing for the `wasm-unknown-unknown` target and `wasm-web` feature is done with the [wasm-bindgen](https://github.com/wasm-bindgen/wasm-bindgen) library.

To install the library:

> cargo install wasm-bindgen-cli

To install the WebDrivers:

- Mozilla Geckodriver (might need to install Firefox for it to work properly):
  - <https://github.com/mozilla/geckodriver/releases> or
  - `cargo install geckodriver` or
  - `sudo apt update` and `sudo apt install firefox-esr geckodriver`
- Chromedriver: <https://chromedriver.chromium.org/downloads> or `sudo apt install chromium-browser chromium-chromedriver`
- Msedgedriver: <https://developer.microsoft.com/en-us/microsoft-edge/tools/webdriver/>

### WASM build and test quickstart

The Rust crate exposes browser-specific functionality behind the `wasm-web` feature flag. Contributors should validate changes against the `wasm32-unknown-unknown` target with the following commands:

1. Ensure the toolchain wasm target is available:

   > rustup target add wasm32-unknown-unknown

2. Check that the workspace compiles for wasm with the web feature enabled:

   > cargo check --target wasm32-unknown-unknown --features wasm-web

3. Run the wasm smoke tests (powered by `wasm-bindgen-test`) in headless mode:

   > cargo test --target wasm32-unknown-unknown --features wasm-web wasm_smoke

The suite in `tests/wasm_smoke.rs` provides a minimal browser-oriented sanity check and should pass before opening a pull request.

For convenience, the repository also ships `./scripts/smoke.sh` (or `scripts\smoke.bat` on Windows) which chains the formatting check, a trimmed native test run (skipping network-bound cases), the wasm `cargo check`, and the wasm smoke test when the `app_check` module is enabled for wasm. If `wasm-bindgen-test-runner` is not installed locally, the script will emit a warning and skip the wasm test step.

## Common AI prompts to develop code/documentation for this library

Detailed instructions for the AI are given in the ./AGENTS.md file. Here are some handy prompts we commonly used to work on the library. It is not an extensive list of the prompts, but so far they have worked fine for us.

For implementing a specific feature you are interested in:

> Following the instructions in ./AGENTS.md, implement the feature {XXX} for the module {module}.

Example: Following the instructions in ./AGENTS.md, implement the StorageReference operations for the module storage.

For moving forward in the porting of a module you are interested in, leaving to the AI to decide what it should work on:

> Following the instructions in ./AGENTS.md, read in the file ./src/{module}/PORTING_STATUS.md what are the next steps and the missing features in the module {module} and work on the first step

For documenting some of the API:

> Following the instructions in ./AGENTS.md, review the Rust code for the module {module} and write or improve the appropriate documentation

For creating an example of some feature you might be interested in:

> Following the instructions in ./AGENTS.md, write an example for the module {module} demonstrating how to use the feature {feature}. Save the example in the folder ./examples with a filename starting with the {module}_ name

For porting some of the tests from the JS SDK library:

> Following the instructions in ./AGENTS.md, review the tests in the Typescript code in ./packages/{module} and port some of the relevant tests to Rust

For a failed test:

> `cargo test [--target wasm-unknown-unknown --features wasm-web]` failed at the test {name_of_test}. Here is the output of the test with the failure message: \[Content of the cargo test output\]

For a bug:

> The module {module} did not work as expected, I suspect a bug. The expected behavior of the following code is \[expected behavior\], but I obtained \[actual behavior\]

For updating the PORTING_STATUS.md of any module:

> Review the Typescript code in ./packages/{module} and the Rust code in ./scr/{module}, and check if ./src/{module}/PORTING_STATUS.md is up to date. Check specifically for the features implemented and the feature still to be implemented. Make the necessary correction to bring the file up to date.

For preparing for a PULL REQUEST:

> Write a title and a message for a pull request explaining in detail what are the changes in the code and the benefits of those changes

For having an estimate of the porting advancement

> Compare the original JS/Typescript files in ./packages/{module} and the ported files in Rust in ./src/{module}, and give me an estimated guess, in percentage, of how much of the features/code of the Firebase JS SDK has been ported to Rust for this module. Update the README.md file accordingly.

## Before any pull request

Before any pull request, the following steps must be taken:

1. Format your code with `cargo fmt`.
2. Ask the AI to update `./src/{module}/README.md` and `./src/{module}/PORTING_STATUS.md` (as mandated in `./AGENTS.md`).
3. Run `./scripts/cargo_check[.bat|.sh]` and `./scripts/cargo_test[.bat|.sh]` and ensure all tests pass.
4. Compile the docs with `cargo doc` and verify there are no errors.
5. Ask the AI to write a pull request title and message, or write them yourself—be specific and precise.

## Bugs and erroneous documentation

Chances are, there are bugs in the code. If you find one, or if you notice that something is not documented correctly, you can open an issue on Github or submit a Pull request.

## Before you Contribute

The code you contribute MUST be licensed under Apache 2.0.

## Testing

In the analytics module a unit test that exercises the dispatcher is skipped by default unless `FIREBASE_NETWORK_TESTS=1` is set.

## Workspace layout

The SDK is a cargo workspace. `crates/firebase-core` holds the app lifecycle, the component
container, credentials and the platform primitives; each product is a crate beside it, and the root
package `firebase-rs-sdk` is a façade that re-exports them behind one feature per product.

```bash
cargo test --workspace              # every crate
cargo test -p firebase-auth         # one product
cargo build --no-default-features --features remote-config   # what a single-product user builds
```

Two rules keep the graph honest, and CI enforces both:

- **Products depend on core, never on each other's internals.** `firebase-functions` depends on
  `firebase-messaging` for the instance-id token, which is a real dependency rather than a cycle;
  apart from that, and from the products that need an installation id, a product's only Firebase
  dependency is `firebase-core`.
- **Each product must build on its own** (`--no-default-features --features <product>`), which is
  what stops a Remote Config user from compiling gRPC.

### What core owns

Four things every product needs live in `firebase-core`, and a product should reach for them
rather than write its own:

- **The service registry** — a product declares its service once by implementing
  `component::Service` (the component name, when it is created, whether an app can hold several
  instances), registers a factory with `app::register_service::<S, _>(factory)`, and resolves it
  back with `app::service_provider::<S>(&app)` or `container.get::<S>()`. The name and the Rust
  type travel together, so a lookup cannot disagree with the registration: asking for a component
  as the wrong type is an error, where the untyped container answered `None` and looked exactly
  like "that product is not installed". Nothing outside core builds a `Component` or calls
  `get_provider` by hand.

- **Credentials** — `platform::credentials::AppCredentials::for_app(&app)` resolves the user's ID
  token and the App Check token from the app's component container, per request. Auth and App Check
  publish themselves into that container as a `TokenSource`, which is what lets Firestore, Storage,
  Functions and the rest attach a token without depending on the crate that produced it. Never look
  up `auth-internal` or `app-check-internal` from a product.
- **Transport** — `platform::http::HttpClient` is the SDK's HTTP client: one implementation for
  native and the browser, with headers, timeouts and a retry policy. A product writes its own
  request code only for something the shared client deliberately does not do — gRPC (Firestore's
  `Listen`), websockets (the Realtime Database), a streaming response body (Storage downloads,
  streaming callables), or a client the caller supplies (the AI request builder).
- **Errors** — `util::status::StatusCode` and `GoogleApiError::from_response` read what a Firebase
  backend said: the canonical `google.rpc` status, the message, and the `{"error": {…}}` envelope
  (including the array wrapper Firestore's streaming RPCs use). Products keep their own public
  error types and map the status onto them.

## Porting from the JavaScript SDK

Every module here is a port of a `packages/**` module in
[firebase-js-sdk](https://github.com/firebase/firebase-js-sdk), and the port is only as good as its
trail back to the original. Three rules keep that trail:

- **Cite the file you read.** A doc comment that says which TypeScript file a behaviour came from
  is what lets the next person check it. `packages/app-check/src/providers.ts`, not "the JS SDK".
- **Copy constants with their reasoning.** A retry budget or a refresh margin is a decision someone
  made with data we do not have. When a value differs from the JS SDK's on purpose, say why in the
  comment and add a row to the table in [`docs/js-sdk-parity.md`](docs/js-sdk-parity.md).
- **Say when you are diverging.** Some things should not be ported — the JS container's `any`
  lookups, App Check's dummy tokens, `gtag.js`. Those belong in the "Deliberate divergences"
  section of the parity document, not in a code comment nobody finds.

To measure where the port stands:

```bash
git clone --depth 1 https://github.com/firebase/firebase-js-sdk
scripts/api_parity.py ../firebase-js-sdk --missing
```

It reads the JS SDK's own API Extractor reports and looks for a counterpart to each public entity
in the matching crate. The match is by name, so treat it as an estimate — but it is an estimate
that moves when the port does.

## Coverage table

Per-module coverage lives in `docs/coverage.toml` and nothing else. README.md's table is generated
from it:

```bash
scripts/coverage_table.py           # rewrite the table
scripts/coverage_table.py --check   # what CI runs
```

Module `README.md` and `PORTING_STATUS.md` files carry porting history, not numbers — they used to
carry their own percentages, which drifted from the README by as much as fifty points.

## Generated Firestore protobufs

Firestore's `Listen` RPC (which powers `on_snapshot`) is gRPC-only, so the crate carries generated
bindings for `google.firestore.v1`:

- `proto/` holds the `.proto` sources, vendored from
  [googleapis](https://github.com/googleapis/googleapis) (Apache-2.0).
- `crates/firebase-firestore/src/remote/proto/` holds the generated Rust, committed so that building
  the crate needs no `protoc`.

After changing anything under `proto/`, regenerate with:

```bash
scripts/generate_firestore_protos.sh   # needs protoc (brew install protobuf)
```

## Live endpoint tests

`tests/live_endpoints.rs` exercises real Firebase backends through the public API. The tests are
`#[ignore]`d so `cargo test` stays offline.

### Against the Local Emulator Suite (default, no credentials)

Auth, Firestore, Realtime Database, Storage and callable Functions run against the Firebase
emulators. One-time setup:

```bash
npm install -g firebase-tools     # needs Node 20+ and Java 11+
npm ci --prefix firebase-emulator/functions   # callable fixtures served by the Functions emulator
```

Then:

```bash
scripts/emulator_test.sh                        # everything
scripts/emulator_test.sh firestore_transaction  # only matching tests
```

The script wraps `firebase emulators:exec` with a `demo-*` project id, so the CLI never contacts
Google and needs no login. The emulated project is defined entirely under `firebase-emulator/`
(`firebase.json`, `firestore.rules`, `database.rules.json`, `storage.rules`, `functions/index.js`);
the rules mirror what the online project uses.
`Auth` also reads `FIREBASE_AUTH_EMULATOR_HOST` on its own at construction, so code started by the
Firebase CLI reaches the Auth emulator without calling `connect_emulator`.
The harness reads the standard `FIREBASE_AUTH_EMULATOR_HOST`, `FIRESTORE_EMULATOR_HOST`,
`FIREBASE_DATABASE_EMULATOR_HOST`, `FIREBASE_STORAGE_EMULATOR_HOST` and
`FIREBASE_FUNCTIONS_EMULATOR_HOST` variables, so any other way of starting the emulators works too.

### App Check

App Check attestation needs a browser, so tests use the debug-token flow. The emulator tests need
nothing: they install a custom provider and check the token arrives on a callable (the `echoHeaders`
fixture reports the headers it received). To exercise the real exchange endpoint against the online
project, register a debug token under App Check > Apps > Manage debug tokens in the console and
export it:

```bash
export FIREBASE_APPCHECK_DEBUG_TOKEN=<the token from the console>
```

The variable also switches any `initialize_app_check` call over to the debug provider, mirroring the
JS SDK's debug mode. Treat it as a credential: anyone holding it can mint App Check tokens for the
project.

### Against the online project (Installations, Remote Config)

Installations and Remote Config have no emulator. They run only when credentials for a real
project are provided, and are skipped otherwise. Credentials are never committed. Provide one of:

- `google-services.json` at the crate root (or `FIREBASE_GOOGLE_SERVICES_FILE=/path`, or the raw
  JSON in `FIREBASE_GOOGLE_SERVICES_JSON`, which is how CI injects the secret);
- a `.env.firebase` dot file with `FIREBASE_API_KEY`, `FIREBASE_PROJECT_ID`, `FIREBASE_APP_ID`
  (plus optional `FIREBASE_PROJECT_NUMBER`, `FIREBASE_STORAGE_BUCKET`, `FIREBASE_DATABASE_URL`,
  `FIREBASE_TEST_CALLABLE`);
- the same variables in the environment.

```bash
cargo test --test live_endpoints -- --ignored --nocapture
```

Without emulator variables, the migrated tests also run against the online project; products that
are not enabled there make the affected test print a `SKIP:` line with the console action needed
and pass. `live_project_probe` prints a one-screen summary of what the credentials can reach.

### CI

`.github/workflows/live-tests.yml` has two jobs: `emulator` (no secrets, runs for every push and
pull request) and `live` (uses the `FIREBASE_GOOGLE_SERVICES_JSON` secret to run the online-only
tests on pushes to `main`, same-repository pull requests, a weekly schedule and manual dispatch).
