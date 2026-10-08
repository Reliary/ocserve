# ocserve-importer

Streaming, read-only importer for `ocserve`: pulls sessions/messages/parts
from an existing opencode `opencode.db` into an ocserve store with
byte-parity payloads and blob spill for large parts.
