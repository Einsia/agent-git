# Windows CLI validation and packaging

GitLab runs native Windows tests in the validation stage for merge requests,
the default branch, and explicitly requested branch pipelines. The
`rust:test:windows` matrix separates library/startup tests from CLI integration
tests. Both jobs must pass before default-branch packaging begins.

Run either job locally on a Windows x64 host with the Rust native build
prerequisites, Node.js, Git, and curl available:

```powershell
./scripts/test-windows-cli.ps1 -Suite unit
./scripts/test-windows-cli.ps1 -Suite integration
```

The jobs bootstrap job-local Rust, Git LFS, and cargo-nextest installations.
The preparation stage runs `windows:test-tools` before the longer validation
jobs can occupy the Linux runners. It downloads the pinned archives from
`scripts/windows-test-tools.json` on Linux, verifies their checksums, and passes
them to both native jobs as GitLab artifacts. Windows verifies them again and
refuses missing or corrupt prepared archives. Local runs can download the tools
directly. Tool archives are cached independently of compiled test binaries;
merge requests only read the shared caches.

`scripts/windows-test-suites.json` owns the native regression selection. Its
`unit` entries are library test-name substrings; startup policy tests also run
from the CLI binary. Its `integration` entries are standard Cargo integration
test targets. The `custom` entries use standalone harnesses without nextest's
listing protocol and run with `cargo test` after nextest in the integration job.
Add Windows-specific regressions here when they need native behavior,
including ACLs, process trees, JSON capture, filesystem paths, and CLI launches.
The ordinary Linux validation job continues to run the full Cargo test suite,
including doctests. Windows cross-compilation with Clippy remains a separate gate.

Each native job invokes nextest once using the test build profile, without
release LTO. The union of library selectors executes each matching test once.
`.config/nextest.toml` limits concurrency, reserves the full pool for deadline
and audit fixtures that require isolation, bounds individual test duration,
and writes a JUnit report uploaded even on failure. Avoid `--nocapture` with
nextest: it disables parallel execution. Use the captured failure output and
JUnit report to diagnose a failing test.

`scripts/package-windows-cli.ps1` builds the release executable and creates the
archive. It checks the executable, embedded version, and npm installation path
against that release artifact. It does not install test tools, run Rust test
suites, or compile the library without default features. The same packaging
script serves dev and staging; the staging child pipeline is reached through
the parent pipeline's validation stage.
