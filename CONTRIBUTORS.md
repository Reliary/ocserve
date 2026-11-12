# Contributors

People who have sent code, a reproduction, or a diagnosis that changed the
project. Listed by GitHub handle, oldest first. See the `Recognition` section of
`CONTRIBUTING.md` for what earns a line here.

| Handle | Contribution |
|---|---|
| [@alderpath](https://github.com/alderpath) | Project author and maintainer. |
| [@tungmv](https://github.com/tungmv) | Reported that `provider.<id>.options.apiKey` was ignored, which produced an empty bearer token (issue #28). The endpoint builder now reads auth, config and the credential table in that order, and resolves `{env:VAR}` and `{file:path}`. |
| [@glanzel](https://github.com/glanzel) | Reported the seven Paseo v2 integration gaps (issue #33) and sent the fix branch (PR #34): config-directory precedence, launcher-injected plugin config, `file://` and directory plugin specs, contract-shaped `Plugin.Info`, `parentID` filtering, plugin `ctx` return values, and the message-list cursor. The cursor commit includes the heap diagnosis (a client walked the repeated page to ~3.4 GB) that made the bug findable. |
