# ocserve

A compatible reimplementation of the [opencode](https://github.com/anomalyco/opencode)
server in Rust: the same HTTP and SSE wire, so the official TUI, the web UI and
the SDK clients work unmodified against it.

```sh
cargo install ocserve
ocserve serve --port 4096
```

## Build prerequisites

BoringSSL is compiled from source (the free-tier lane needs its TLS
fingerprint), so the build needs a C toolchain:

- Debian/Ubuntu: `apt install build-essential cmake perl pkg-config`
- Fedora: `dnf install gcc gcc-c++ make cmake perl pkgconf-pkg-config`
- macOS: `xcode-select --install` and `brew install cmake`
- Windows: the MSVC build tools plus `cmake` and `perl`

Rust 1.98 or newer. The embedded web UI is compiled in, so no extra download
happens at runtime.

## Commands

`serve` (the drop-in server), `import`, `backup`, `doctor`, `models`, `env-reference`,
`replay` (differential wire harness), and the self-check commands.

Full documentation, the compatibility ledger and the divergence list live in the
[repository](https://github.com/Reliary/ocserve).
