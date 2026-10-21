# Real-plugin compatibility corpus

Answers one question mechanically: **does a real third-party OpenCode plugin
load and run under ocserve?**

The plugin test suite in `crates/ocserve-plugin` covers every *authoring form*
with synthetic fixtures. It cannot tell you whether the ecosystem actually
works, because a fixture is written by the same person who wrote the loader.
This corpus is the independent half: real packages, published on npm, pinned by
exact version.

## Method

For each package in `manifest.json`:

1. **Resolve the entry** exactly the way `crates/ocserve-plugin` does:
   `exports["."]` (string, or an object whose `import`/`default` is a string),
   then `main`, then `module`. A nested condition object is *not* an entry — that
   is ocserve's rule, not Node's, so the harness must not use Node's resolution.
2. **Load it** through the real sidecar host (`host.mjs`), sending the same
   NDJSON `load` request the server sends.
3. **Record the registered hooks.**
4. **Invoke each registered v1 hook** with a synthesized payload and record
   whether it ran, mutated the output, or threw.
5. **Deliver one synthetic bus event** through the v1 `event` channel.

Dependencies are installed with `--ignore-scripts` (a postinstall hook is
arbitrary code and the fetch stage has network), peers are pinned to the SDK the
server actually implements (`@opencode-ai/plugin@1.18.31`,
`@opencode/plugin@2.0.28`), and each install is time-bounded. A package whose
tree will not install inside the budget is reported as `missing-dependency`; it
is not a compatibility verdict.

## Results legend

| class | meaning |
|---|---|
| `hooks-ran` | loaded, registered v1 hooks, every invoked hook ran |
| `v2-registered` | loaded as a v2 plugin and registered domain hooks (registration only; v2 dispatch is domain-typed and needs shape-specific inputs) |
| `registered-only` | loaded and registered hooks, but none of them are in the invoked set (for example `auth`) |
| `loaded-no-hooks` | loaded and returned no hooks at all |
| `load-error` | our loader rejected it — **this is the class that matters** |
| `load-timeout` | setup did not finish in 20 s with the network off; likely awaits a fetch |
| `missing-dependency` | a dependency of the package could not be installed (or is not published); says nothing about ocserve |
| `entry-error` | the package declares no entry ocserve's rule accepts |

`load-error` rows are the actionable ones. A `missing-dependency` row is a
statement about npm, not about compatibility, and is reported separately for
that reason.

## Sandbox

Plugin code is arbitrary. The execution stage runs with:

```
--network none            no egress
--read-only               no writes outside tmpfs
--user 65534:65534        non-root
--tmpfs /tmp              writable HOME
--memory 1g --pids-limit 256 --cpus 1.5
--cap-drop ALL --security-opt no-new-privileges
timeout 900               hard wall clock
```

One fresh sidecar per plugin, so a plugin that corrupts the host or hangs cannot
affect the next result.

## Running

```sh
# fetch (network on) + run (network off) locally
scripts/plugin-corpus.sh

# iterate on the harness without re-installing dependencies
scripts/plugin-corpus.sh --run-only

# execute on the sandbox host instead (corpus is built here and copied over,
# because that host has no DNS by design)
scripts/plugin-corpus-remote.sh
```

The corpus is gitignored and ephemeral. Only `report.json` is committed.

## Limits, stated plainly

- **Registration is not effect.** A plugin that loads and registers has proven
  its code can run in our host. It has not proven its hook *changes the prompt*,
  because that needs a model turn and a real session. The v2 shim in particular
  only bridges a subset of domains; those gaps are named `D-V2-*` divergences in
  `crates/ocserve-plugin/src/host/README-v2.md`, and a v2 plugin using an
  unbridged domain shows up here as `load-error` with that divergence in the
  message — which is the intended, honest outcome.
- **Twenty packages is a sample.** npm reports roughly 119 packages declaring
  the v1 SDK and 117 declaring v2. This set was chosen to cover v1-only,
  v2-only, both, and neither, not to be representative by count.
- **Synthetic payloads.** Hook inputs are minimal but shape-plausible. A plugin
  that requires a field we did not synthesize may throw here and work in a real
  session. The `threw:` text is recorded so that is visible rather than averaged
  away.
