// The vanilla (no-framework) demo: pick a BDMV folder, run the scan in a Worker,
// show the live progress and the rendered disc report. No upload — everything
// stays in the browser.
import { analyze, type BdmvFile } from "./analyze.js";

const picker = document.getElementById("picker") as HTMLInputElement;
const status = document.getElementById("status") as HTMLElement;
const report = document.getElementById("report") as HTMLElement;

picker.addEventListener("change", async () => {
  const list = picker.files;
  if (list === null || list.length === 0) {
    return;
  }

  const files: BdmvFile[] = Array.from(list, (file) => ({
    path: file.webkitRelativePath || file.name,
    file,
  }));

  picker.disabled = true;
  report.textContent = "";
  status.textContent = `Scanning ${files.length} files…`;

  try {
    const text = await analyze(files, ({ file, done, total }) => {
      const pct = total > 0 ? Math.floor((done / total) * 100) : 0;
      status.textContent = `Scanning ${file} — ${pct}%`;
    });
    status.textContent = text.length > 0 ? "Done." : "No readable Blu-ray structure found.";
    report.textContent = text;
  } catch (error) {
    status.textContent = `Error: ${error instanceof Error ? error.message : String(error)}`;
  } finally {
    picker.disabled = false;
  }
});
