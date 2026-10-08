# ocserve-llm

Streaming provider client for `ocserve`: OpenAI-compatible chat
completions over SSE with chunk-split-safe parsing (`SseLineParser`),
usage/finish capture, tool-call assembly, zen keyless-mode support, and a
fuzz corpus proving chunk-boundary invariance.

Provider behavior differences are handled by shape, not by model lists:
the auth header and base URL are the routing key.
