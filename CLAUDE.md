Read `AGENTS.md` in this folder; it names the shared rule files to read first.

Its **No loops** rule (owner, 2026-10-29) binds every Claude session and every job you start: the owner's goal outranks the gates, at most two fix rounds per PR, flaky CI in untouched code is re-run once and recorded, never chain flake jobs, and report a problem that returns a third time instead of starting another round.
