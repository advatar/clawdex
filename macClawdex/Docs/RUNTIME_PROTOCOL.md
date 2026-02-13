# Runtime protocol (suggested) — clawdex ui-bridge --stdio

The starter macOS app talks to `clawdex` via **newline-delimited JSON** over stdin/stdout.

## App → clawdex (stdin)

### User message
```json
{"type":"user_message","text":"Hello"}
```

### Subscribe to streamed turn events
```json
{"type":"subscribe_events","subscriptionId":"ui","kinds":["turn_started","turn_completed"]}
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

### Streamed event for a subscriber
```json
{"type":"ui_event","subscriptionId":"ui","eventKind":"turn_completed","event":{}}
```

## Notes

- Keep stdout strictly JSONL so the UI can parse it reliably.
- Send diagnostic logs to stderr (the UI still captures them, but treats them as logs).
