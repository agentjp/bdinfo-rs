// Builds the WebAssembly artifact for the browser package: compile the
// independent `bdinfo-rs-wasm` crate to `wasm32-unknown-unknown` (release), then
// run wasm-bindgen `--target web` to emit `pkg/` (the `.wasm`, the ESM glue, and
// the `.d.ts`). Cross-platform; mirrors the hand-rolled pipeline the gate uses.
import { execSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const crate = resolve(here, ".."); // crates/bdinfo-rs-wasm
const wasm = resolve(crate, "target/wasm32-unknown-unknown/release/bdinfo_rs_wasm.wasm");
const pkg = resolve(here, "pkg");

const run = (cmd, opts) => execSync(cmd, { stdio: "inherit", ...opts });

run("cargo build --release --target wasm32-unknown-unknown", { cwd: crate });
run(`wasm-bindgen --target web --out-dir "${pkg}" --out-name bdinfo_rs_wasm "${wasm}"`);

console.log(`built ${pkg}`);
