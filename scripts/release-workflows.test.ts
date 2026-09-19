import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";

const load = (name: string): any =>
  Bun.YAML.parse(readFileSync(new URL(`../.github/workflows/${name}.yml`, import.meta.url), "utf8"));
const preview = load("preview");
const release = load("release");
const adminGate = release.jobs.release.steps[0];

describe("publishing workflow boundaries", () => {
  test("releases are tag-only, previews are dispatch-only, and PR CI stays enabled", () => {
    expect(release.on).toEqual({ push: { tags: ["v*"] } });
    // This fork publishes previews by hand rather than from a `preview-*` tag,
    // so the boundary is the trigger itself: workflow_dispatch is restricted to
    // accounts with write access, and neither a PR nor a branch push can reach it.
    expect(Object.keys(preview.on)).toEqual(["workflow_dispatch"]);
    expect(load("ci").on.pull_request).toBeDefined();
  });

  test("preview checks do not require a workstation Windows SDK", () => {
    const checks = preview.jobs.preflight.steps.find((step: any) => step.name === "Run checks");
    expect(checks.run.trim().split("\n")).toEqual(["just ci", "just docs-contract-test"]);
    expect(preview.jobs.build.strategy.matrix.include).toContainEqual({
      target: "x86_64-pc-windows-msvc",
      os: "windows-latest",
      name: "herdr-windows-x86_64.zip",
    });
    expect(preview.jobs.publish.needs).toContain("build");
  });

  test("the job that publishes a release rechecks both actors before using credentials", () => {
    // `release` is the only publishing job that can run on this fork: the
    // others are gated on `github.repository == 'herdrdev/herdr'`, which is
    // permanently false here. Whatever else changes, the job that actually
    // uploads assets must still gate on admin permission first.
    const job = release.jobs.release;
    expect(job.steps[0]).toEqual(adminGate);
    expect(adminGate.run).toContain('"$GITHUB_ACTOR" "$GITHUB_TRIGGERING_ACTOR"');
    expect(adminGate.env.GH_TOKEN).toBe("${{ github.token }}");
    expect(adminGate.run).not.toContain("ogulcancelik");
  });

  test("release recipes are maintainer-local and never invoked by a workflow", () => {
    // The release recipes interpolate their `version` argument into shell text,
    // which is safe only because nothing automated ever passes them input. If a
    // workflow ever calls one, that argument becomes injectable and this test
    // must be replaced by a real non-interpolation assertion.
    for (const name of ["release", "preview", "ci"]) {
      const text = readFileSync(new URL(`../.github/workflows/${name}.yml`, import.meta.url), "utf8");
      expect(text).not.toMatch(/just\s+(release|release-prepare|release-publish)\b/);
    }
    for (const recipe of ["release-prepare", "release-publish", "release"]) {
      expect(spawnSync("just", ["--dry-run", recipe, "0.0.0"], { encoding: "utf8" }).status).toBe(0);
      // One version argument, not two: the recipes take `version` alone.
      expect(spawnSync("just", ["--dry-run", recipe, "0.0.0", "0.0.0"], { encoding: "utf8" }).status).not.toBe(0);
    }
  });

  test.skipIf(process.platform === "win32")("admin gate permits admins and fails closed for other roles or API errors", () => {
    const dir = mkdtempSync("/var/tmp/herdr-admin-gate-");
    try {
      writeFileSync(join(dir, "gh"), `#!/bin/sh
case "$2" in
  */collaborators/admin-*/permission) echo admin ;;
  */collaborators/maintainer/permission) echo maintain ;;
  */collaborators/writer/permission) echo write ;;
  *) exit 1 ;;
esac
`, { mode: 0o755 });
      for (const [actor, trigger, succeeds] of [
        ["admin-one", "admin-two", true],
        ["writer", "admin-two", false],
        ["admin-one", "writer", false],
        ["admin-one", "maintainer", false],
        ["admin-one", "api-error", false],
      ] as const) {
        const result = spawnSync("bash", ["-c", adminGate.run], {
          env: { ...process.env, PATH: `${dir}:${process.env.PATH}`, GITHUB_REPOSITORY: "example/test", GITHUB_ACTOR: actor, GITHUB_TRIGGERING_ACTOR: trigger },
          encoding: "utf8",
        });
        expect(result.status === 0).toBe(succeeds);
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
