// Use already-built binaries. This runner never invokes a build or a test.
import { resolve, join } from "node:path";
import { mkdir } from "node:fs/promises";

const [qaPath, hostPath, outputPath = "native-page-proof"] = Bun.argv.slice(2);
if (!qaPath || !hostPath) throw new Error("usage: bun scripts/native-page-witness.ts <ps-qa> <native-page-host> [output-dir]");
const qa = resolve(qaPath);
const host = resolve(hostPath);
const output = resolve(outputPath);
await mkdir(output, { recursive: true });
const root = resolve(import.meta.dir, "..");
const fixture = join(root, "qa/ekojit-page/index.html");
const checks = join(root, "qa/ekojit-page/checks.ron");
type Report = { backend: string; native_entries: number; failures: number;
  checks: { id: string; verdict: string }[] };

async function run(name: string, plant?: string): Promise<{ code: number; report: Report }> {
  const reportPath = join(output, `${name}.json`);
  if (await Bun.file(reportPath).exists()) throw new Error(`proof report already exists: ${reportPath}`);
  const args = [qa, "native-page", "--host", host, "--page", fixture,
    "--checks", checks, "--engine", "ekojit", "--report", reportPath];
  if (plant) args.push("--plant", plant);
  const child = Bun.spawn(args, { stdout: "pipe", stderr: "pipe" });
  let timedOut = false;
  const deadline = setTimeout(() => { timedOut = true; child.kill("SIGKILL"); }, 30_000);
  const [code, stdout, stderr] = await Promise.all([child.exited,
    new Response(child.stdout).text(), new Response(child.stderr).text()]);
  clearTimeout(deadline);
  await Bun.write(join(output, `${name}.stdout`), stdout);
  await Bun.write(join(output, `${name}.stderr`), stderr);
  if (timedOut) throw new Error(`${name}: harness deadline is not a named failure`);
  if (!(await Bun.file(reportPath).exists())) throw new Error(`${name}: missing outcome report`);
  const report = await Bun.file(reportPath).json() as Report;
  const expected = ["ekojit-native-entry", "ekojit-page-ready", "ekojit-document-title", "ekojit-console-log", "ekojit-text-append"];
  if (report.backend !== "ekojit" || report.native_entries < 1
    || report.checks.length !== expected.length
    || expected.some(id => report.checks.filter(check => check.id === id).length !== 1)) {
    throw new Error(`${name}: invalid backend, native receipt or check manifest`);
  }
  return { code, report };
}

const healthy = await run("healthy");
if (healthy.code !== 0 || healthy.report.failures !== 0
  || healthy.report.checks.some(check => check.verdict !== "pass")) throw new Error("healthy native page failed");
const planted = await run("host-text-append-noop", "host-text-append-noop");
if (planted.code !== 1 || planted.report.failures !== 1
  || planted.report.checks.some(check => check.verdict !== (check.id === "ekojit-text-append" ? "fail" : "pass"))) {
  throw new Error("host-text-append-noop did not fail only ekojit-text-append");
}
console.log("PASS ekojit-page: native hello page passed; host-text-append-noop failed ekojit-text-append");
