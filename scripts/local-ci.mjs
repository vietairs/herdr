#!/usr/bin/env node
// Generic local CI runner (zero dependencies, Node >= 18, ESM). Copy verbatim to scripts/local-ci.mjs.
// Runs the legs in scripts/ci/legs.json against a fresh clone of HEAD (linux legs in an Apple
// `container`, macos legs natively, windows legs never) and can post per-leg GitHub commit statuses.
// Invariants: the user's working tree is never executed; zero legs run => never green;
// --post only ever vouches for a pushed, clean, drift-free HEAD.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const OSES = ["linux", "macos", "windows"];
const LEG_KEYS = ["check", "os", "script", "image", "arch", "setup", "env"];
const ARCHES = ["amd64", "arm64"];
// First existing path wins; .github/ci/ serves repos that forbid a root scripts/ dir.
const MANIFEST_PATHS = ["scripts/ci/legs.json", ".github/ci/legs.json"];

export class UsageError extends Error {}

// ---------- args ----------
export function parseArgs(argv) {
  const o = { list: false, legs: [], os: null, dryRun: false, post: false, allowDirty: false, keep: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const val = () => {
      if (i + 1 >= argv.length) throw new UsageError(`${a} needs a value`);
      return argv[++i];
    };
    if (a === "--list") o.list = true;
    else if (a === "--leg") o.legs.push(val());
    else if (a === "--os") {
      o.os = val();
      if (!["linux", "macos"].includes(o.os)) throw new UsageError("--os must be linux or macos");
    } else if (a === "--dry-run") o.dryRun = true;
    else if (a === "--post") o.post = true;
    else if (a === "--allow-dirty") o.allowDirty = true;
    else if (a === "--keep") o.keep = true;
    else throw new UsageError(`unknown argument: ${a}`);
  }
  return o;
}

// ---------- manifest ----------
// Returns every problem found (not just the first) so one run fixes the whole file.
export function validate(manifest, root) {
  const errs = [];
  if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) return ["manifest must be an object"];
  if (typeof manifest.repo !== "string" || !/^[\w.-]+\/[\w.-]+$/.test(manifest.repo)) errs.push('"repo" must be "owner/name"');
  if (!Array.isArray(manifest.legs) || manifest.legs.length === 0) return [...errs, '"legs" must be a non-empty array'];
  const seen = new Set();
  manifest.legs.forEach((leg, i) => {
    const at = `legs[${i}]${leg && leg.check ? ` (${leg.check})` : ""}`;
    if (!leg || typeof leg !== "object") return errs.push(`${at}: must be an object`);
    for (const k of Object.keys(leg)) if (!LEG_KEYS.includes(k)) errs.push(`${at}: unknown key "${k}"`);
    if (typeof leg.check !== "string" || !leg.check.trim()) errs.push(`${at}: "check" must be a non-empty string`);
    else if (seen.has(leg.check)) errs.push(`${at}: duplicate check name "${leg.check}"`);
    else seen.add(leg.check);
    if (!OSES.includes(leg.os)) errs.push(`${at}: unknown os "${leg.os}" (use ${OSES.join("|")})`);
    // Script is interpolated into a shell string, so keep it to a safe path alphabet inside the repo.
    if (typeof leg.script !== "string" || !/^[\w./-]+$/.test(leg.script) || leg.script.split("/").includes("..")) {
      errs.push(`${at}: "script" must be a repo-relative path without spaces or ".."`);
    } else if (!fs.existsSync(path.join(root, leg.script))) errs.push(`${at}: script file missing: ${leg.script}`);
    if (leg.os === "linux" && (typeof leg.image !== "string" || !leg.image)) errs.push(`${at}: linux leg needs "image"`);
    if (leg.arch !== undefined && !(leg.os === "linux" && ARCHES.includes(leg.arch))) {
      errs.push(`${at}: "arch" is linux-only, one of ${ARCHES.join("|")}`);
    }
    if (leg.setup !== undefined && typeof leg.setup !== "string") errs.push(`${at}: "setup" must be a string`);
    if (leg.env !== undefined) {
      const ok = leg.env && typeof leg.env === "object" && !Array.isArray(leg.env) &&
        Object.entries(leg.env).every(([k, v]) => /^[A-Za-z_]\w*$/.test(k) && typeof v === "string");
      if (!ok) errs.push(`${at}: "env" must map NAME to string values`);
    }
  });
  return errs;
}

export function loadManifest(root) {
  const rel = MANIFEST_PATHS.find((p) => fs.existsSync(path.join(root, p)));
  if (!rel) throw new UsageError(`${MANIFEST_PATHS.join(" or ")} not found in ${root}`);
  const file = path.join(root, rel);
  let manifest;
  try { manifest = JSON.parse(fs.readFileSync(file, "utf8")); }
  catch (e) { throw new UsageError(`${rel} is not valid JSON: ${e.message}`); }
  const errs = validate(manifest, root);
  if (errs.length) throw new UsageError(`${rel} invalid:\n  - ${errs.join("\n  - ")}`);
  return manifest;
}

// A leg whose script no workflow mentions can pass locally while GitHub runs something else.
export function findDrift(manifest, root) {
  const dir = path.join(root, ".github", "workflows");
  const text = fs.existsSync(dir)
    ? fs.readdirSync(dir).filter((f) => /\.ya?ml$/.test(f)).map((f) => fs.readFileSync(path.join(dir, f), "utf8")).join("\n")
    : "";
  return manifest.legs.filter((l) => !text.includes(l.script)).map((l) => l.check);
}

// ---------- plan ----------
const shq = (s) => (/^[\w@%+=:,./-]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`);
const innerScript = (leg) => `${leg.setup ? `${leg.setup}; ` : ""}bash ${leg.script}`;

export function buildContainerCommand(leg, { clone, cpus = "4", memory = "8G" }) {
  const env = Object.entries(leg.env || {}).flatMap(([k, v]) => ["-e", `${k}=${v}`]);
  return {
    cmd: "container",
    args: ["run", "--rm", "--arch", leg.arch || "amd64", "--cpus", String(cpus), "--memory", String(memory),
      "-v", `${clone}:/work`, "-w", "/work", "-e", "CI=true", ...env,
      leg.image, "bash", "-euo", "pipefail", "-c", innerScript(leg)],
  };
}

const slugify = (s) => s.replace(/[^\w.-]+/g, "_");

// One item per selected leg: action "run" (cmd/args/cwd/env/logFile) or "skip" (reason). Windows never runs.
export function buildPlan(manifest, { clone, logDir, opts = {}, env = process.env, platform = process.platform }) {
  const sel = manifest.legs.filter((l) =>
    (!opts.legs || !opts.legs.length || opts.legs.some((s) => l.check === s || l.check.includes(s))) &&
    (!opts.os || l.os === opts.os));
  return sel.map((leg) => {
    const base = { check: leg.check, os: leg.os, logFile: path.join(logDir, `${slugify(leg.check)}.log`) };
    if (leg.os === "windows") return { ...base, action: "skip", reason: "runs on the Windows runner via GitHub" };
    if (leg.os === "macos") {
      if (platform !== "darwin") return { ...base, action: "skip", reason: `needs a macOS host (this is ${platform})` };
      return { ...base, action: "run", cwd: clone, env: { ...env, CI: "true", ...(leg.env || {}) },
        cmd: "bash", args: leg.setup ? ["-euo", "pipefail", "-c", innerScript(leg)] : [leg.script] };
    }
    const { cmd, args } = buildContainerCommand(leg, { clone, cpus: env.LOCAL_CI_CPUS || "4", memory: env.LOCAL_CI_MEMORY || "8G" });
    return { ...base, action: "run", cwd: clone, env, cmd, args };
  });
}

// ---------- exec / posting ----------
// exec(cmd, args, {cwd, env, logFile}) -> {status, stdout, stderr}; injectable for tests.
export function defaultExec(cmd, args, { cwd, env, logFile } = {}) {
  const fd = logFile ? fs.openSync(logFile, "w") : null;
  try {
    const r = spawnSync(cmd, args, { cwd, env, encoding: "utf8", maxBuffer: 64 << 20,
      stdio: fd === null ? ["ignore", "pipe", "pipe"] : ["ignore", fd, fd] });
    return { status: r.status ?? (r.error ? 127 : 1), stdout: r.stdout || "", stderr: r.stderr || (r.error ? r.error.message : "") };
  } finally { if (fd !== null) fs.closeSync(fd); }
}

// Decide whether statuses may be posted. Returns refusal reasons ([] = allowed).
export function postPlan({ repo, sha, exec, dirty, drift = [], allowDirty = false }) {
  const why = [];
  if (allowDirty) why.push("--allow-dirty runs never post");
  if (dirty) why.push("tracked working tree is dirty");
  if (drift.length) why.push(`DRIFT: no workflow references the script of: ${drift.join(", ")}`);
  if (exec("gh", ["api", `repos/${repo}/commits/${sha}`]).status !== 0) why.push(`HEAD ${sha.slice(0, 12)} is not on ${repo} (push it first)`);
  return why;
}

const postStatus = (exec, repo, sha, check, state, description) =>
  exec("gh", ["api", "-X", "POST", `repos/${repo}/statuses/${sha}`, "-f", `state=${state}`, "-f", `context=${check}`, "-f", `description=${description}`]);

// ---------- run ----------
// post = {repo, sha} to publish statuses. Skipped legs are never posted.
export function executePlan(plan, { exec, post = null, now = Date.now, log = console.log }) {
  let containerReady = null;
  return plan.map((item) => {
    const r = { check: item.check, os: item.os, status: "SKIP", seconds: 0, reason: item.reason, logFile: item.logFile };
    if (item.action === "skip") return r;
    if (item.cmd === "container" && containerReady === null) containerReady = exec("container", ["system", "status"]).status === 0;
    if (item.cmd === "container" && !containerReady) {
      log(`  ${item.check}: apple container is not running — fix: container system start`);
      return { ...r, status: "FAIL", reason: "container system not running" }; // infra failure: no verdict, nothing posted
    }
    if (post) postStatus(exec, post.repo, post.sha, item.check, "pending", `local-ci ${item.os} running`);
    log(`>>> ${item.check}`);
    const t0 = now();
    const res = exec(item.cmd, item.args, { cwd: item.cwd, env: item.env, logFile: item.logFile });
    r.seconds = Math.round((now() - t0) / 1000);
    r.status = res.status === 0 ? "PASS" : "FAIL";
    if (post && postStatus(exec, post.repo, post.sha, item.check, r.status === "PASS" ? "success" : "failure", `local-ci ${item.os} ${r.seconds}s`).status !== 0) {
      r.postFailed = true; // the verdict never reached GitHub: the run must not exit green
      log(`  ${item.check}: posting the status to GitHub failed`);
    }
    return r;
  });
}

export function summarize(results) {
  const n = (s) => results.filter((r) => r.status === s).length;
  const lines = results.map((r) => `${r.status.padEnd(4)}  ${r.check}${r.status === "SKIP" ? `  (${r.reason})` : `  ${r.seconds}s`}`);
  const ran = n("PASS") + n("FAIL");
  lines.push(`${n("PASS")} passed, ${n("FAIL")} failed, ${n("SKIP")} skipped`);
  if (ran === 0) lines.push("no legs ran — nothing verified");
  return lines.join("\n");
}

// 0 only if at least one leg ran and none failed (never vacuously green).
export function exitCode(results) {
  const ran = results.filter((r) => r.status !== "SKIP");
  return ran.length > 0 && ran.every((r) => r.status === "PASS" && !r.postFailed) ? 0 : 1;
}

// ---------- main ----------
function prepareClone({ repoRoot, sha, slug, exec }) {
  const base = path.join(process.env.TMPDIR || os.tmpdir(), "local-ci", slug, sha);
  const clone = path.join(base, "src");
  fs.rmSync(base, { recursive: true, force: true });
  fs.mkdirSync(base, { recursive: true });
  const steps = [["git", ["clone", "--quiet", "--no-hardlinks", repoRoot, clone], undefined],
    ["git", ["checkout", "--quiet", sha], clone]];
  if (fs.existsSync(path.join(repoRoot, ".gitmodules"))) steps.push(["git", ["submodule", "update", "--init", "--recursive"], clone]);
  for (const [cmd, args, cwd] of steps) {
    const r = exec(cmd, args, { cwd });
    if (r.status !== 0) throw new UsageError(`${cmd} ${args.join(" ")} failed: ${r.stderr.trim()}`);
  }
  return { base, clone };
}

export function main(argv, { exec = defaultExec, cwd = process.cwd(), env = process.env, platform = process.platform, log = console.log } = {}) {
  try {
    const opts = parseArgs(argv);
    const git = (...a) => exec("git", a, { cwd });
    const top = git("rev-parse", "--show-toplevel");
    if (top.status !== 0) throw new UsageError("not inside a git repository");
    const repoRoot = top.stdout.trim();
    const sha = git("rev-parse", "HEAD").stdout.trim();
    const dirty = git("status", "--porcelain", "--untracked-files=no").stdout.trim() !== "";
    const inspectOnly = opts.list || opts.dryRun;
    if (!inspectOnly && dirty && !opts.allowDirty) throw new UsageError("tracked working tree is dirty; commit/stash or pass --allow-dirty (HEAD is what gets tested; such runs never post)");

    // list/dry-run read the working tree; real runs read the manifest from the HEAD clone.
    const slug = slugify(path.basename(repoRoot));
    const prepared = inspectOnly ? null : prepareClone({ repoRoot, sha, slug, exec });
    const root = prepared ? prepared.clone : repoRoot;
    const manifest = loadManifest(root);
    const drift = findDrift(manifest, root);
    drift.forEach((c) => log(`DRIFT: no .github/workflows file references the script of "${c}"`));

    if (opts.list) {
      manifest.legs.forEach((l) => log(`${l.os.padEnd(7)} ${l.check}  ${l.script}${drift.includes(l.check) ? "  [DRIFT]" : ""}`));
      return 0;
    }
    const post = opts.post && !opts.dryRun;
    if (opts.post && opts.dryRun) log("--dry-run: nothing will be posted");
    if (post) {
      const why = postPlan({ repo: manifest.repo, sha, exec, dirty, drift, allowDirty: opts.allowDirty });
      if (why.length) throw new UsageError(`refusing to post:\n  - ${why.join("\n  - ")}`);
    }

    const base = prepared ? prepared.base : path.join(process.env.TMPDIR || os.tmpdir(), "local-ci", slug, sha);
    const plan = buildPlan(manifest, { clone: path.join(base, "src"), logDir: base, opts, env, platform });
    if (!plan.length) throw new UsageError("no leg matches the given --leg/--os filters");

    if (opts.dryRun) {
      for (const p of plan) log(p.action === "skip" ? `SKIP  ${p.check} (${p.reason})` : `RUN   ${p.check}: ${[p.cmd, ...p.args].map(shq).join(" ")}`);
      return 0;
    }
    const results = executePlan(plan, { exec, post: post ? { repo: manifest.repo, sha } : null, log });
    log(`\n${summarize(results)}\nlogs: ${base}`);
    for (const r of results) if (r.status === "FAIL" && r.logFile && fs.existsSync(r.logFile)) log(`--- tail of ${r.check}\n${fs.readFileSync(r.logFile, "utf8").split("\n").slice(-25).join("\n")}`);
    if (prepared && !opts.keep) fs.rmSync(prepared.clone, { recursive: true, force: true });
    return exitCode(results);
  } catch (e) {
    if (!(e instanceof UsageError)) throw e;
    console.error(`local-ci: ${e.message}`);
    return 2;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2));
}
