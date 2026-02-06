-- 001_create_plugin_marketplaces.sql
-- New tables to support Claude-style plugin marketplaces and richer error reporting.
--
-- NOTE: Current Clawdex TaskStore uses "CREATE TABLE IF NOT EXISTS" and no schema versioning.
-- You can either:
--   (A) paste these into TaskStore::migrate(), or
--   (B) run them manually once, then keep them as documentation.
--
-- All timestamps are ms since epoch.

CREATE TABLE IF NOT EXISTS plugin_marketplaces (
  name            TEXT PRIMARY KEY,
  source          TEXT NOT NULL,         -- user input: ./path OR owner/repo OR https://...
  kind            TEXT NOT NULL,         -- 'path' | 'github' | 'url'
  pinned_ref      TEXT,                  -- optional branch/tag
  added_at_ms     INTEGER NOT NULL,
  updated_at_ms   INTEGER NOT NULL,
  last_sync_at_ms INTEGER,
  etag            TEXT,                  -- for HTTP cache validation (if URL-based)
  raw_json        TEXT                   -- original marketplace.json (optional for debugging)
);

CREATE TABLE IF NOT EXISTS marketplace_plugins (
  marketplace_name TEXT NOT NULL,
  plugin_name      TEXT NOT NULL,
  description      TEXT,
  version          TEXT,
  author_json      TEXT,
  category         TEXT,
  tags_json        TEXT,
  strict           INTEGER NOT NULL DEFAULT 1,
  source_json      TEXT NOT NULL,        -- PluginSource (string or object)
  entry_json       TEXT NOT NULL,        -- full plugin entry (forward compat)
  updated_at_ms    INTEGER NOT NULL,

  PRIMARY KEY (marketplace_name, plugin_name),
  FOREIGN KEY (marketplace_name) REFERENCES plugin_marketplaces(name) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS plugin_errors (
  id              TEXT PRIMARY KEY,
  plugin_id       TEXT,                  -- nullable (marketplace errors may not be plugin-specific)
  marketplace     TEXT,                  -- nullable
  scope           TEXT NOT NULL,          -- 'user' | 'project' | 'local' | 'managed'
  kind            TEXT NOT NULL,          -- 'install' | 'manifest' | 'hooks' | 'mcp' | 'lsp' | 'sync' | ...
  message         TEXT NOT NULL,
  details_json    TEXT,
  created_at_ms   INTEGER NOT NULL,
  resolved_at_ms  INTEGER
);

-- Optional convenience index for UI "errors" tab.
CREATE INDEX IF NOT EXISTS idx_plugin_errors_unresolved ON plugin_errors(resolved_at_ms);
