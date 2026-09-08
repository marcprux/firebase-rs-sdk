#!/usr/bin/env bash
# Runs the live endpoint tests against the Firebase Local Emulator Suite.
#
# Usage: scripts/emulator_test.sh [extra cargo test args]
#   scripts/emulator_test.sh                       # every migrated test
#   scripts/emulator_test.sh firestore_transaction # only matching tests
#
# Requirements: Java 11+, Node 20+, `npm install -g firebase-tools`, and
# `npm ci --prefix firebase-emulator/functions` once (installs the callable fixture's dependencies).
#
# The emulator configuration lives in firebase-emulator/ (firebase.json, rules, functions).
#
# `firebase emulators:exec` exports FIREBASE_AUTH_EMULATOR_HOST, FIRESTORE_EMULATOR_HOST,
# FIREBASE_DATABASE_EMULATOR_HOST and FIREBASE_STORAGE_EMULATOR_HOST for the wrapped command;
# tests/live_endpoints.rs routes Auth, Firestore, Realtime Database and Storage through them and
# Functions through FIREBASE_FUNCTIONS_EMULATOR_HOST.
# Installations and Remote Config have no emulator and are skipped unless real credentials are
# configured (see CONTRIBUTING.md).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EMULATOR_DIR="$ROOT/firebase-emulator"

if ! command -v firebase >/dev/null 2>&1; then
  echo "firebase CLI not found; install it with: npm install -g firebase-tools" >&2
  exit 1
fi
if [ ! -d "$EMULATOR_DIR/functions/node_modules" ]; then
  echo "installing callable fixture dependencies (firebase-emulator/functions/node_modules)..." >&2
  npm ci --prefix "$EMULATOR_DIR/functions"
fi

# A demo-* project id keeps the CLI fully offline: no login, no real project is ever touched.
PROJECT_ID="${FIREBASE_EMULATOR_PROJECT:-demo-firebase-rs-sdk}"
export FIREBASE_FUNCTIONS_EMULATOR_HOST="127.0.0.1:5001"
export FIREBASE_EMULATOR_PROJECT_ID="$PROJECT_ID"

# The CLI resolves firebase.json (and the paths inside it) from the current directory, so run it
# from the emulator directory and hop back to the crate root for cargo.
cd "$EMULATOR_DIR"
exec firebase emulators:exec \
  --only auth,firestore,database,storage,functions \
  --project "$PROJECT_ID" \
  "cd '$ROOT' && cargo test --test live_endpoints -- --ignored --nocapture --test-threads=2 $*"
