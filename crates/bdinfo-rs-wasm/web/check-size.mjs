// Size-budget ratchet for the optimized browser payload. Reads the committed
// ceiling from ../wasm-size-budget.txt and fails if the built, wasm-opt'd
// pkg/bdinfo_rs_wasm_bg.wasm exceeds it. Run after `npm run build:wasm`.
// Used by both scripts/wasm-compliance.ps1 (local gate) and wasm.yml (CI), so
// the budget lives in ONE tracked file.
import { readFileSync, statSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const budgetPath = resolve(here, "../wasm-size-budget.txt");
const wasmPath = resolve(here, "pkg/bdinfo_rs_wasm_bg.wasm");

// The budget file is comment lines (`#`) + one integer line.
const budgetLine = readFileSync(budgetPath, "utf8")
  .split(/\r?\n/)
  .map((line) => line.trim())
  .find((line) => line.length > 0 && !line.startsWith("#"));
const budget = Number.parseInt(budgetLine ?? "", 10);
if (!Number.isFinite(budget) || budget <= 0) {
  console.error(`check-size: no valid integer budget in ${budgetPath}`);
  process.exit(2);
}

const size = statSync(wasmPath).size;
const pct = ((size / budget) * 100).toFixed(1);
if (size > budget) {
  console.error(
    `check-size: FAIL — optimized wasm is ${size} B, over the ${budget} B budget (${pct}%).\n` +
      "  Investigate the growth; lower or (with a written reason) raise wasm-size-budget.txt.",
  );
  process.exit(1);
}

console.log(`check-size: OK — optimized wasm ${size} B / ${budget} B budget (${pct}%).`);
