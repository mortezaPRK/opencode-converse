import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, normalize, posix, resolve, win32 } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { build } from "../native/scripts/build.mjs";

const native = fileURLToPath(new URL("../native/", import.meta.url));

function assertNativeSearchPath(output, expected, normalizePath = normalize) {
  const actual = output.match(/^cargo:rustc-link-search=native=([^\r\n]+)$/m)?.[1];
  assert.ok(actual, output);
  assert.equal(normalizePath(actual), normalizePath(expected), output);
}

test("compiler probe path assertion accepts equivalent Windows separators but rejects other paths", () => {
  const windows = "cargo:rustc-link-search=native=C:\\compiler resources\\lib/darwin\r\n";
  assertNativeSearchPath(
    windows,
    win32.join("C:\\compiler resources", "lib", "darwin"),
    win32.normalize,
  );
  assert.throws(() =>
    assertNativeSearchPath(
      windows,
      win32.join("C:\\other compiler", "lib", "darwin"),
      win32.normalize,
    ),
  );
  const unix = "cargo:rustc-link-search=native=/compiler resources/lib/darwin\n";
  assertNativeSearchPath(unix, posix.join("/compiler resources", "lib", "darwin"), posix.normalize);
});

test("build helper prepares Cargo's effective target-specific dependency directories", () => {
  const temp = mkdtempSync(join(tmpdir(), "converse-build-test-"));
  try {
    const targets = [
      ["aarch64-apple-darwin", "aarch64-apple-darwin"],
      ["x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc"],
      [join(temp, "toolchain", "custom.json"), "custom"],
      [undefined, undefined],
    ];
    for (const [index, [target, targetName]] of targets.entries()) {
      const artifactRoot = join(temp, `artifacts-${index}`);
      const env = { CARGO_TARGET_DIR: "relative-artifacts", CARGO_BUILD_TARGET: target };
      const calls = [];
      const result = build({
        env,
        spawn(command, args, options) {
          calls.push(args[0]);
          assert.equal(command, "cargo");
          assert.ok(args.includes("--locked"));
          assert.equal(args[args.indexOf("--manifest-path") + 1], join(native, "Cargo.toml"));
          assert.equal(options.env.CARGO_BUILD_TARGET, target);
          assert.equal(options.env.CARGO_TARGET_DIR, "relative-artifacts");
          if (args[0] === "metadata") {
            assert.equal(options.encoding, "utf8");
            return { status: 0, stdout: JSON.stringify({ target_directory: artifactRoot }) };
          }
          assert.equal(args[0], "build");
          assert.equal(options.stdio, "inherit");
          assert.ok(args.includes("--release"));
          assert.equal(args[args.indexOf("-p") + 1], "opencode-converse");
          assert.equal(args[args.indexOf("--features") + 1], "native-audio");
          for (const profile of ["debug", "release"]) {
            assert.ok(existsSync(join(artifactRoot, profile, "deps")));
            if (targetName) assert.ok(existsSync(join(artifactRoot, targetName, profile, "deps")));
          }
          return { status: 0 };
        },
      });
      assert.equal(result, 0);
      assert.deepEqual(calls, ["metadata", "build"]);
      assert.deepEqual(env, { CARGO_TARGET_DIR: "relative-artifacts", CARGO_BUILD_TARGET: target });
    }
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
});

test("both compiler-rt scripts track CC and preserve wrapped and quoted compiler probes", () => {
  const temp = mkdtempSync(join(tmpdir(), "converse-cc-test-"));
  try {
    // Compile the actual build scripts directly, without Cargo dependencies.
    // The mocked macOS probes never build ORT, open devices, or load models.
    const probeSource = join(temp, "probe.rs");
    writeFileSync(
      probeSource,
      `fn main() {
        let args: Vec<_> = std::env::args().skip(1).collect();
        assert_eq!(args.last().unwrap(), "-print-resource-dir");
        if let Ok(compiler) = std::env::var("EXPECTED_WRAPPED_COMPILER") {
            assert_eq!(args[0], compiler);
            assert_eq!(args[1], "--extra=space value");
            assert_eq!(args.len(), 3);
        } else { assert_eq!(args.len(), 1); }
        println!("{}", std::env::var("PROBE_RESOURCE_DIR").unwrap());
    }`,
    );
    const suffix = process.platform === "win32" ? ".exe" : "";
    const compiler = join(temp, `mock compiler${suffix}`);
    const wrapper = join(temp, `mock wrapper${suffix}`);
    const compileProbe = spawnSync("rustc", [probeSource, "-o", compiler], { encoding: "utf8" });
    assert.equal(compileProbe.status, 0, compileProbe.stderr);
    writeFileSync(wrapper, readFileSync(compiler), { mode: 0o755 });
    const resource = join(temp, "compiler resources");
    const library = join(resource, "lib", "darwin");
    mkdirSync(library, { recursive: true });
    writeFileSync(join(library, "libclang_rt.osx.a"), "");
    const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
    for (const crate of ["ve-vad", "ve-turn"]) {
      const source = resolve(native, "crates", crate, "build.rs");
      assert.ok(readFileSync(source, "utf8").includes('var("CC")'));
      const executable = join(temp, `${crate}${suffix}`);
      const compile = spawnSync("rustc", [source, "-o", executable], { encoding: "utf8" });
      assert.equal(compile.status, 0, compile.stderr);
      const tests = join(temp, `${crate}-tests${suffix}`);
      const compileTests = spawnSync("rustc", ["--test", source, "-o", tests], {
        encoding: "utf8",
      });
      assert.equal(compileTests.status, 0, compileTests.stderr);
      const unitTests = spawnSync(tests, [], { encoding: "utf8" });
      assert.equal(unitTests.status, 0, unitTests.stdout + unitTests.stderr);
      const run = spawnSync(executable, [], {
        encoding: "utf8",
        env: { ...process.env, CARGO_CFG_TARGET_OS: "linux", CC: "fixture-clang" },
      });
      assert.equal(run.status, 0, run.stderr);
      assert.match(run.stdout, /^cargo:rerun-if-env-changed=CC$/m);
      for (const [cc, wrapped] of [
        [compiler, false],
        [quote(compiler), false],
        [JSON.stringify(compiler), false],
        [`${quote(wrapper)} ${quote(compiler)} ${quote("--extra=space value")}`, true],
      ]) {
        const env = {
          ...process.env,
          CARGO_CFG_TARGET_OS: "macos",
          CC: cc,
          PROBE_RESOURCE_DIR: resource,
        };
        if (wrapped) env.EXPECTED_WRAPPED_COMPILER = compiler;
        else delete env.EXPECTED_WRAPPED_COMPILER;
        const probe = spawnSync(executable, [], { encoding: "utf8", env });
        assert.equal(probe.status, 0, probe.stderr);
        assertNativeSearchPath(probe.stdout, library);
        assert.match(probe.stdout, /^cargo:rustc-link-lib=static=clang_rt.osx$/m);
      }
    }
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
});
