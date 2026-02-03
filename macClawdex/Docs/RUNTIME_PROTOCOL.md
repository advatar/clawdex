# Runtime protocol (suggested) — clawdex ui-bridge --stdio

The starter macOS app talks to `clawdex` via **newline-delimited JSON** over stdin/stdout.

## App → clawdex (stdin)

### User message
```json
{"type":"user_message","text":"Hello"}
```

(Extend as needed)
- `thread` (string) — optional thread/session key
- `route`  — optional delivery route key (channel/to)

## clawdex → App (stdout)

### Assistant message
```json
{"type":"assistant_message","text":"Hi! How can I help?"}
```

### Error
```json
{"type":"error","message":"Something went wrong"}
```

## Notes

- Keep stdout strictly JSONL so the UI can parse it reliably.
- Send diagnostic logs to stderr (the UI still captures them, but treats them as logs).
