# Windows CLI validation and packaging

GitLab runs native Windows tests in the validation stage for merge requests,
the default branch, and explicitly requested branch pipelines. The
`rust:test:windows` job builds the native regression selection together and must
pass before default-branch packaging begins.

Run the job locally on a Windows x64 host with the Rust native build
prerequisites, Node.js, Git, and curl available:

```powershell
./scripts/test-windows-cli.ps1
```

The jobs bootstrap job-local Rust, Git LFS, and cargo-nextest installations.
The preparation stage runs `windows:test-tools` before the longer validation
jobs can occupy the Linux runners. It downloads the pinned archives from
`scripts/windows-test-tools.json` on Linux, verifies their checksums, and passes
them to the native job as GitLab artifacts. Windows verifies them again and
refuses missing or corrupt prepared archives. Local runs can download the tools
directly. Tool archives are cached independently of compiled test binaries;
merge requests only read the shared caches.

`scripts/windows-test-suites.json` owns the native regression selection. Its
`unit` entries are library test-name substrings; startup policy tests also run
from the CLI binary. Its `integration` entries are standard Cargo integration
test targets. Standalone provider-repair matrices run in Linux validation.
Add Windows-specific regressions here when they need native behavior,
including ACLs, process trees, JSON capture, filesystem paths, and CLI launches.
Do not add platform-independent business or presentation matrices to this list.
Use `-Suite unit` or `-Suite integration` for focused local runs.
Linux validation runs shared unit tests, core CLI integrations, and product Clippy.
See [CI validation scope](ci-validation.md) for the core suite and full local command. Native Windows tests compile the
Windows code directly; CI does not also download an SDK to cross-compile it.

The native job invokes nextest once using the test build profile, without
release LTO, so library and integration tests share the same dependency build.
The union of library selectors executes each matching test once.
`.config/nextest.toml` limits concurrency, bounds individual test duration,
and writes a JUnit report uploaded even on failure. Avoid `--nocapture` with
nextest: it disables parallel execution. Use the captured failure output and
JUnit report to diagnose a failing test.

`scripts/package-windows-cli.ps1` builds the release executable and creates the
archive. It checks the executable, embedded version, and npm installation path
against that release artifact. It does not install test tools, run Rust test
suites, or compile the library without default features. The same packaging
script serves dev and staging; the staging child pipeline is reached through
the parent pipeline's validation stage.
