# Firebase Local Emulator Suite configuration

Everything needed to run `tests/live_endpoints.rs` against local emulators instead of a real
Firebase project:

- `firebase.json`: emulator ports and the rules/functions locations (paths are relative to this
  directory).
- `firestore.rules`, `storage.rules`: security rules for the scratch collection / folder the tests
  use. They mirror the rules published on the online project.
- `functions/`: callable Cloud Functions fixtures served by the Functions emulator
  (`helloWorld`, `alwaysFails`). Install its dependencies once with `npm ci --prefix firebase-emulator/functions`.

Run the suite with `scripts/emulator_test.sh` from the repository root; see `CONTRIBUTING.md`.
