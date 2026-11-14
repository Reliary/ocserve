# Contributors

People who have sent code, a reproduction, or a diagnosis that changed the
project. Listed by GitHub handle, oldest first. See the `Recognition` section of
`CONTRIBUTING.md` for what earns a line here.

| Handle | Contribution |
|---|---|
| [@alderpath](https://github.com/alderpath) | Project author and maintainer. |
| [@tungmv](https://github.com/tungmv) | Reported that `provider.<id>.options.apiKey` was ignored, which produced an empty bearer token (issue #28). The endpoint builder now reads auth, config and the credential table in that order, and resolves `{env:VAR}` and `{file:path}`. |
| [@glanzel](https://github.com/glanzel) | Reported the seven Paseo v2 integration gaps (issue #33) and sent the fix branch (PR #34): config-directory precedence, launcher-injected plugin config, `file://` and directory plugin specs, contract-shaped `Plugin.Info`, `parentID` filtering, plugin `ctx` return values, and the message-list cursor. The cursor commit includes the heap diagnosis (a client walked the repeated page to ~3.4 GB) that made the bug findable. |
| [@jrzmurray](https://github.com/jrzmurray) | Three correctness branches merged after review: permission request wire shapes (PR #40), v2 drain cancellation and session-delete fencing (PR #41), and goal-review cancellation on delete (PR #42). Each found a real class the maintainers had not covered; the amendments on top (ledger rows, bounded teardown, off-worker admission, a retry owner for deferred reviews) are recorded in the merge commits. |
