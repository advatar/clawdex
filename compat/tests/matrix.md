# Compatibility Test Matrix

## Tool coverage (P0)
- cron.add
- cron.update
- cron.list
- cron.remove
- memory_search
- memory_get
- message.send
- heartbeat.wake

## Tool coverage (P1)
- cron.run
- cron.runs
- cron.status
- channels.list
- channels.resolve_target

## Scenario tests
- Cron job persists across daemon restart
- Cron main-session run injects a system event and triggers a Codex turn
- Cron isolated run creates its own Codex thread and can deliver output
- Heartbeat runs on interval and suppresses delivery when response == HEARTBEAT_OK
- Memory search returns file + line ranges and can recall MEMORY.md content
- Last-route delivery works when channel/to omitted
