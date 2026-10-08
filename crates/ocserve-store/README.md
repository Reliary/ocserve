# ocserve-store

SQLite + chunked blob storage for `ocserve`: the single shared pragma
profile for every connection role, the STRICT schema with step-loop
migrations, the single-writer worker with typed `WriteOp`s, zstd blob
spill with fsync+rename crash protocol, and the trigram FTS5 search
projection.

All storage rules live in `STORAGE.md` in the repository root (pragma
profiles per role, cache budgets, backup/`VACUUM INTO` drills). Read paths
use thread-parked readers; write paths funnel through one writer thread.
