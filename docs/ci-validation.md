# CI validation scope

Keep tests that pin public behavior, authorization boundaries, durable state,
recovery, or platform-specific I/O. A test should distinguish a plausible broken
implementation without requiring the internal layout to remain unchanged.

Do not multiply each business case by every output format. Cover presentation
contracts with focused output tests and use representative formats for business
scenarios. Avoid separate tests for fixture helpers, trivial argument forwarding,
or documentation formatting when they repeat existing coverage.

Linux runs workspace unit tests, product Clippy, and core CLI integration tests
for session selection, publication, merge/recovery, credentials, and RC. The
standalone tunnel process test also runs. Use `cargo test --locked --workspace`
locally when changing presentation, status, or another non-core integration path;
those matrices are not mandatory for every commit. Native Windows tests cover Windows
security, paths, process ownership, pipes, startup, and installation. macOS runs
product Clippy and process/signal regressions. Neither platform repeats the full
Linux business matrix. Packaging builds artifacts and verifies their identity;
it does not rerun the complete test suite under release optimization.

CI omits disposable debug symbols. Dev CLI packages use parallel code generation
without LTO; staging and public release optimization remain defined by their
release configuration. Validation caches downloaded Rust sources by platform,
branch, and lockfile and saves them after successful jobs, so MR commits reuse
their new dependencies without writing another branch's source cache. Build
outputs stay outside those caches. Process and deployed-cloud acceptance scripts stay
explicit because they require isolated daemons and authenticated real harnesses.

Use JUnit reports and retained daemon logs to investigate failures. Keep
credentials and conversation bodies out of diagnostic logs. Do not increase a
suite timeout to hide an unbounded fixture or duplicate work.
