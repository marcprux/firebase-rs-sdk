#!/usr/bin/env bash
# Regenerates crates/firebase-firestore/src/remote/proto/ from the vendored protos in proto/.
#
# The generated code is committed so that building the crate needs no `protoc`; run this only when
# the vendored `.proto` files change. Requires `protoc` (brew install protobuf / apt install
# protobuf-compiler).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/Cargo.toml" <<'TOML'
[package]
name = "firestore-protogen"
version = "0.0.0"
edition = "2021"

[dependencies]
tonic = "0.11"
prost = "0.12"
prost-types = "0.12"

[build-dependencies]
tonic-build = "0.11"
TOML

mkdir -p "$WORK/src"
echo 'pub mod v1 { include!(concat!(env!("OUT_DIR"), "/google.firestore.v1.rs")); }' > "$WORK/src/lib.rs"
cat > "$WORK/build.rs" <<BUILD
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    tonic_build::configure()
        .build_server(false)
        .out_dir(&out)
        .compile(&["google/firestore/v1/firestore.proto"], &["$ROOT/proto"])?;
    std::fs::write(out.join("OUT_DIR_PATH"), out.display().to_string())?;
    Ok(())
}
BUILD

(cd "$WORK" && cargo build --quiet 2>/dev/null || true)
OUT="$(find "$WORK/target" -name 'google.firestore.v1.rs' | head -1)"
if [ -z "$OUT" ]; then
  echo "code generation failed; run 'cargo build' in $WORK to see why" >&2
  exit 1
fi
GEN_DIR="$(dirname "$OUT")"
DEST="$ROOT/crates/firebase-firestore/src/remote/proto"
# The upstream proto comments contain fenced code blocks that rustdoc would try to run as
# doctests, so the doc comments are dropped on the way in; the .proto files under proto/ stay the
# reference documentation.
strip_doc_comments() {
  grep -v '^[[:space:]]*///' "$1" > "$2"
}
strip_doc_comments "$GEN_DIR/google.firestore.v1.rs" "$DEST/firestore_v1.rs"
strip_doc_comments "$GEN_DIR/google.rpc.rs" "$DEST/rpc.rs"
strip_doc_comments "$GEN_DIR/google.r#type.rs" "$DEST/type_latlng.rs"
echo "regenerated $DEST from $ROOT/proto"
