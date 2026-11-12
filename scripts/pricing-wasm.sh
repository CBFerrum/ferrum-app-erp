#!/bin/sh
# Builds ferrum-erp-pricing for the browser (`priceBasket`, JSON in, JSON out) and prints the files' SHA-256 digests,
# so a build elsewhere can be compared with this one.
#   scripts/pricing-wasm.sh [--check] [out-dir]     (out-dir: $CARGO_TARGET_DIR/pricing-wasm, else target/pricing-wasm)
# Needs the wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown) and wasm-bindgen-cli at the
# version the workspace pins, the one Ferrum-Frontend's @ferrum/calc is built with
# (cargo install --locked wasm-bindgen-cli --version 0.2.129). Local paths are kept out of the binary.
# --check also prices pricing/tests/baskets.jsonl through the WebAssembly with Node and compares each answer, byte for
# byte, with pricing/tests/priced.jsonl, what the native crate gives (its test `fixtures_are_priced_byte_for_byte`).
set -e
check=
[ "$1" = "--check" ] && { check=1; shift; }
root=$(cd "$(dirname "$0")/.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
out=${1:-$target/pricing-wasm}
pinned=$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' "$root/Cargo.toml")
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}')
[ "$have" = "$pinned" ] || { echo "wasm-bindgen-cli $pinned is needed (found: ${have:-none})" >&2; exit 1; }
(cd "$root" && RUSTFLAGS="--remap-path-prefix=$root=. --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=cargo" \
  cargo rustc -q -p ferrum-erp-pricing --release --features wasm --target wasm32-unknown-unknown --crate-type cdylib)
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" --out-name ferrum_erp_pricing "$target/wasm32-unknown-unknown/release/ferrum_erp_pricing.wasm"
(cd "$out" && if command -v sha256sum >/dev/null; then sha256sum ferrum_erp_pricing*; else shasum -a 256 ferrum_erp_pricing*; fi)
[ -n "$check" ] || exit 0
node --input-type=module -e '
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
const [out, tests] = process.argv.slice(1);
const glue = await import(pathToFileURL(out + "/ferrum_erp_pricing.js"));
glue.initSync({ module: readFileSync(out + "/ferrum_erp_pricing_bg.wasm") });
const asked = readFileSync(tests + "/baskets.jsonl", "utf8").trim().split("\n");
const kept = readFileSync(tests + "/priced.jsonl", "utf8").trim().split("\n");
let bad = 0;
asked.forEach((line, i) => { if (glue.priceBasket(line) !== kept[i]) { bad++; console.error("basket " + i + " differs"); } });
if (asked.length !== kept.length || bad) process.exit(1);
console.log(asked.length + " baskets: the WebAssembly answers byte for byte as the native crate");
' "$out" "$root/pricing/tests"
