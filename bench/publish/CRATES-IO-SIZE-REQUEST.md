# crates.io upload-size increase request

Status: sent by the maintainer 2026-10-20 (per their report); awaiting reply.
Superseded figures below are from 2026-10-21; 0.6.8 packed `ocserve-http` at 37,332,357 B before sharding.

- To: the crates.io help address (`help` at the crates.io domain)
- Subject: Upload size increase for `ocserve-http` (34 MiB packed)
- Owner account: `alderpath`
- Crate: https://crates.io/crates/ocserve-http
- Repo: https://github.com/Reliary/ocserve (MIT)

Measured facts behind the request (2026-10-21):

| item | value |
|---|---|
| default limit (crates.io `src/config/publish_limits.rs`) | 10,485,760 B (10 MiB) |
| `ocserve-http` 0.3.1 as published | 5,315,755 B |
| `ocserve-http` 0.6.8 packed (pre-shard) | 37,332,357 B (35.6 MiB) |
| of which `assets/webui/2.0.28.pack.zst` | 24.41 MiB |
| of which `assets/webui/1.18.31.pack.zst` | 9.70 MiB |
| other 10 workspace crates, packed, combined | 0.54 MiB |

The packs are already zstd-compressed per-file frames, so the crate gzip
cannot shrink them: gzip -6 = 35,919,096 B, gzip -9 = 35,923,107 B.

---

## Email body

Copy from the four-backtick fence.

````
Hello crates.io team,

I would like to request a larger upload size for the crate `ocserve-http`.

**Crate:** `ocserve-http`
**Account:** `alderpath`
**Requested limit:** 64 MiB (67,108,864 bytes)
**Current packed size:** 35,919,096 bytes (34.26 MiB)

### Why it is large

`ocserve-http` is the HTTP layer of ocserve, an API-compatible
reimplementation of the opencode server. To serve the web UI without any
outbound network access, the crate embeds two version-matched web UI bundles
as build-time assets:

- `assets/webui/2.0.28.pack.zst` — 24.41 MiB
- `assets/webui/1.18.31.pack.zst` — 9.70 MiB

Each pack is a set of individually zstd-compressed files (a per-file container,
not one compressed blob), because the server decompresses only the assets a
browser actually requests and keeps them in a bounded cache. That is why the
packs do not recompress: the packaged crate measures 35,919,096 bytes at gzip
level 6 and 35,923,107 bytes at level 9, against 35.6 MiB uncompressed.

The assets are embedded rather than downloaded at runtime on purpose. The
server is used on private networks and in air-gapped setups, so a runtime fetch
from a CDN would break its main deployment guarantee. They are also pinned per
upstream release, so that the UI a client receives always matches the API
contract that release was tested against.

### Scope

- This is one crate. The other ten crates in the workspace are small
  (combined packed size 0.54 MiB); the largest is `ocserve-core` at 0.20 MiB.
- The crate contains no vendored C or binary toolchains, only the two asset
  packs plus the Rust source.
- The repository is MIT licensed and the bundled UI assets are redistributed
  under their upstream MIT license, with attribution retained in
  `THIRD-PARTY.md`.

### Current state

`0.3.1` published at 5,315,755 bytes, before the second UI pack was added.
`0.4.1` is tagged and released on GitHub, and publishing it to crates.io is
blocked by the default limit with:

```
max upload size is: 10485760
```

64 MiB would cover this release with headroom for one more pinned UI pack; I
am happy to justify again if it ever needs to grow beyond that.

Thank you,
alderpath
````
Send from the address registered with the `alderpath` crates.io account.
