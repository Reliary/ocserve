## What this changes

<!-- One paragraph. What was wrong, what is different now. -->

## Issue

Closes #

## Verification

<!-- Paste the commands you ran. See CONTRIBUTING.md for the full gate set. -->

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
./scripts/check-guards.sh
```

## Checklist

- [ ] A bug fix has a test that fails without the fix (negative control)
- [ ] A `crates/**/*.rs` change stages a `TRACEABILITY.md` row
- [ ] New behaviour is named in `CHANGELOG.md` if it is user-visible
- [ ] No personal names, email addresses or absolute home paths
- [ ] Commits are conventional (`feat:`, `fix:`, `docs:`, `test:`, `chore:`)
