#!/usr/bin/env node
import { readFile, stat } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";

const paths = process.argv.slice(2);
if (paths.length === 0) {
  console.error("Usage: node scripts/summarize-run.mjs <run-directory|last-run.json> [...]");
  process.exitCode = 1;
} else {
  try {
    for (const path of paths) {
      const absolute = resolve(path);
      const file = (await stat(absolute)).isDirectory() ? join(absolute, "last-run.json") : absolute;
      const report = JSON.parse(await readFile(file, "utf8"));
      const milliseconds = (value) => typeof value === "number" ? (value / 1_000).toFixed(3) : "unavailable";
      const count = (value) => typeof value === "number" ? String(value) : "unavailable";
      const timing = (label, key) => {
        const value = report[key];
        return `${label.padEnd(24)} ${milliseconds(value?.p50).padStart(12)} ${milliseconds(value?.p99).padStart(12)} ${count(value?.count).padStart(10)}`;
      };

      console.log(`\n${basename(dirname(file))} (${file})`);
      console.log(`${"Timing".padEnd(24)} ${"p50 ms".padStart(12)} ${"p99 ms".padStart(12)} ${"samples".padStart(10)}`);
      console.log(timing("All slot intervals", "slotPacingUs"));
      console.log(timing("Within-window intervals", "withinWindowPacingUs"));
      console.log(timing("Window handover gap", "windowHandoverGapUs"));
      console.log(timing("Commit spread", "commitSpreadUs"));
      console.log(timing("Cblock finalization", "finalizationUs"));
      console.log(`Finalized cblocks: ${count(report.finalizedCblocks)}; skipped cblocks: ${count(report.skippedCblocks)}`);
      console.log(`Execution lag from production: maximum ${count(report.maximumExecutionLagSlots)} slots`);
      console.log(`Execution skew between validators: maximum ${count(report.maximumExecutionSkewSlots)} slots; final ${count(report.currentExecutionSkewSlots)} slots`);
      console.log(`State hashes: ${count(report.allValidatorsStateHashHeights)} heights observed at all validators; ${count(report.comparedStateHashHeights)} compared; ${count(report.mismatchingStateHashHeights)} mismatches`);
      console.log(`Last executed slot by validator: ${Array.isArray(report.validatorLastExecuted) ? report.validatorLastExecuted.map((value) => value ?? "none").join(", ") : "unavailable"}`);
    }
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
