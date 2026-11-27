# ocserve-containment

One handle-verified path-containment implementation shared by the HTTP file
routes and the tool layer.

Every path is resolved to an OS handle and the *handle* is checked against the
base directory, so a symlink swap between the check and the use cannot escape:

- Linux: `openat2` with `RESOLVE_BENEATH | NO_MAGICLINKS`
- macOS: `F_GETPATH` on the opened fd, then a path-prefix check
- Windows: `GetFinalPathNameByHandleW` plus `CompareStringOrdinal`

Absolute paths are rebased onto the canonical root instead of being trusted, and
missing parent directories are created one verified component at a time
(`mkdirat` from the previous fd) so a symlinked component is refused before
anything is created through it.
