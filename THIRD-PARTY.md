# Third-party notices

## opencode

`ocserve` is an independent reimplementation of the opencode server API
(clean wire compatibility; original Rust source). The upstream project is
distributed under the MIT License, whose terms (retained below) permit
derivative works, modification, and redistribution provided the copyright
notice and permission notice are retained:

```
MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Upstream project: `github.com/anomalyco/opencode`.

## Embedded prompt assets

`crates/ocserve-cli/assets/*.txt` (initialize, review, compaction, explore,
summary, title) and `customize-opencode.txt` are prompt/skill bodies that
originate from the upstream project's MIT-licensed source
(`packages/core/src/plugin/skill/customize-opencode.md` and the v1 prompt
templates) or are captured from its wire responses; the notice above covers
them. They are embedded so the built-in commands and skills behave
byte-equivalently to the frozen upstream.

## Embedded web UI

`bench/webui/app/<tag>.pack.zst` is the built `packages/app` web client,
captured from the pinned upstream build through its public HTTP interface
(`scripts/extract-app.sh`) and embedded via `include_bytes!` so ocserve serves
the fully version-matched UI offline (upstream parity: the upstream binary
embeds its own `app/dist`). It is MIT-licensed upstream code, covered by the
opencode notice above; regenerated only on a deliberate upstream version bump.
