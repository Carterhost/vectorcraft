# VectorCraft AI Assistant

VectorCraft includes an optional native AI Assistant panel backed by the user's local Codex app-server.

## Local use

1. Launch VectorCraft normally.
2. Open **Window → AI Assistant**.
3. Enable **Allow external prompts** only when a trusted local bridge should be allowed to submit prompts.
4. Type a design request in the panel, or send a JSON-lines control request to the existing control port:

```json
{"id":1,"method":"assistant.inspect","params":{}}
{"id":2,"method":"assistant.prompt","params":{"prompt":"Inspect the active artwork and improve the visual hierarchy."}}
```

The assistant receives the current document inspection, the registered engine/UI command catalog, the available tools, and available panels. Returned commands are validated and executed on the UI thread, so edits remain native and undoable.

## Codex discovery

VectorCraft searches `VECTORCRAFT_CODEX_PATH`, `PATH`, `~/.local/bin/codex`, `~/.antigravity-ide/bin/codex`, `/opt/homebrew/bin/codex`, and `/usr/local/bin/codex`. No API key is compiled into the app.

## macOS artifact

The `ai-assistant-macos.yml` workflow builds an unsigned `VectorCraft.app` on a GitHub-hosted macOS runner and uploads a zip artifact. Signing/notarization remains a repository-owner release concern and is not required for local testing.
