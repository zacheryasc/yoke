## REPL probing

If the code exposes REPL-accessible boundaries (CLI commands, HTTP endpoints, library APIs, shell scripts), open an interactive session and use it to probe the implementation directly.

- **Exercise every boundary** — call each exposed function/endpoint/command with normal inputs first, then adversarial ones.
- **Chain operations** — does state from one call corrupt the next? Try create→read→update→delete sequences and variations.
- **Interrupt mid-flow** — Ctrl-C during an operation, kill a session mid-transaction. Does it recover?
- **Explore discoverability** — can you figure out how to use the interface without reading the source? Are help/usage messages accurate?

Document the REPL session. If something broke, paste the exact input and output.
