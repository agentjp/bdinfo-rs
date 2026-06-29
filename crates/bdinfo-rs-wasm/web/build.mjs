// Builds the WebAssembly artifact for the browser package: compile the
// independent `bdinfo-rs-wasm` crate to `wasm32-unknown-unknown` (release), run
// wasm-bindgen `--target web` to emit `pkg/` (the `.wasm`, the ESM glue, and the
// `.d.ts`), then shrink the `.wasm` with `wasm-opt -Oz` in place. Cross-platform;
// mirrors the hand-rolled pipeline the gate (scripts/wasm-compliance.ps1) runs.
import { execFileSync, execSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const crate = resolve(here, ".."); // crates/bdinfo-rs-wasm
const wasm = resolve(crate, "target/wasm32-unknown-unknown/release/bdinfo_rs_wasm.wasm");
const pkg = resolve(here, "pkg");
const bg = resolve(pkg, "bdinfo_rs_wasm_bg.wasm");

// The binaryen `wasm-opt` from the pinned devDependency (no global install). It
// ships as a Node script, so run it with `node` directly — cross-platform, and
// it sidesteps Node's refusal to spawn the `.cmd` shim without `shell: true`.
const wasmOpt = resolve(here, "node_modules/binaryen/bin/wasm-opt");

execSync("cargo build --release --target wasm32-unknown-unknown --locked", {
  cwd: crate,
  stdio: "inherit",
});
execSync(`wasm-bindgen --target web --out-dir "${pkg}" --out-name bdinfo_rs_wasm "${wasm}"`, {
  stdio: "inherit",
});
// `--all-features`: rustc's wasm32 output uses sign-ext / bulk-memory / etc., so
// wasm-opt must accept those features or it rejects the module as invalid.
execFileSync(process.execPath, [wasmOpt, "-Oz", "--all-features", bg, "-o", bg], {
  stdio: "inherit",
});

console.log(`built ${pkg}`);
