# Tracked git hooks

Enable once per clone:

```sh
git config core.hooksPath .githooks
```

The hook is `pre-commit` (AGENTS §3): the banned-string check (runtime-derived
login token via `whoami`, absolute home paths, personal emails) and the
docs-required check (staged `crates/*.rs` must carry its `TRACEABILITY.md` row,
unless `REFINE_SKIP_DOCS=1` for test-only/refactor commits).

The script contains no literal personal values; if it ever fails on a line you
believe is legitimate, fix the file or extend the documented exclusion list in
the same commit (the exclusion list governs, per `no-personal-name.md`).
