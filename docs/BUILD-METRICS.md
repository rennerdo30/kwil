# Build and CI measurements (KW-35)

`CI` records measurements on GitHub-hosted Linux, Windows and macOS runners
for each PR/main commit, on manual dispatch, and every Monday. No local
measurement builds or external service are needed.

- `build metrics (<OS>)` publishes a job summary and a `build-metrics-<OS>`
  artifact: JSON, Markdown, build logs, and one Cargo `--timings` HTML per phase.
- `CI wall time and weekly trend` publishes `ci-metrics`: JSON and Markdown
  containing per-job wall times (including setup/cache post steps) and a weekly
  trend. It also compares test-job durations with the green CI run on the PR
  base commit (otherwise latest green main), naming both commits/runs. These
  are single-run observations, subject to cache and runner variation.
  Artifacts expire after 90 days.
- The trend reads the latest successful **main** artifact from each of the last
  eight ISO weeks, plus the current run. Same-week values use the median. The
  first run seeds history. It scans up to 500 recent `ci-metrics` artifacts;
  expired/unavailable history is not invented. PR samples never become shared
  history. Downloaded ZIPs are read in memory, never extracted or executed.

Build samples use a fresh target directory, no compiler cache, four Cargo jobs,
incremental compilation and the repository's dev profile (`line-tables-only`).
Dependency fetching is outside the timer. The clean build is
`cargo build --workspace --locked --offline --timings -j 4`. Each incremental
sample adds an anonymous private constant to `core-text/src/lib.rs` or
`ui-gpu/src/lib.rs` and reruns that command. Source bytes are restored even if
a build fails. An untimed baseline rebuild after core-text restoration keeps
the UI sample independent. These are small source-invalidation measurements,
not predictions for arbitrary feature edits. Target bytes are logical file
sizes including incremental data, not compressed cache size or disk allocation.

Hosted runners are not the owner PC; SPEC §9's owner-PC budgets are reference
values, not pass/fail thresholds for these samples. The measurement job refuses
to run locally, with a compiler wrapper, or against an existing target folder.

CI keeps the same `cargo test --workspace --locked` selection, including
doctests and existing ignored-test semantics. All mutation proofs and smoke
commands remain intact. Clippy's unchanged workspace/all-targets command runs
in parallel on all three OSes instead of delaying test execution. Test check
names remain stable; the new clippy checks must also be green before merging.
Existing rust-cache, sccache and test debug-info settings are retained.
