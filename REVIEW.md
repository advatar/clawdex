# Code Review: clawdex

Review date: 2026-05-11
Tracker: https://github.com/advatar/Tracker/issues/53
Scope: top-level app folder `clawdex` and nested project manifests under this folder, excluding generated dependency/build directories such as `.git`, `node_modules`, `target`, `.build`, `dist`, and virtual environments.

## Executive Summary

- Overall risk from this sweep: **Medium**
- Findings by severity: High 0, Medium 3, Low 1
- Source footprint: 84 source files by extension scan (Rust 58, Swift 21, Shell 3, HTML 1, SQL 1)
- Test footprint: 7 test-like files detected
- CI footprint: 2 GitHub Actions workflow files detected
- Git posture: 1 changed/untracked paths before review generation
- Pattern scan budget used: 157 text/source files scanned

## Architecture Snapshot

Detected project and build surfaces:
- `clawdex/Cargo.toml`
- `clawdex/share/Cargo.toml`

Nested manifest owners sampled:
- `clawdex`
- `clawdex/share`

Package scripts sampled:
- No JavaScript package scripts detected.

Local instruction/status files:
- `AGENTS.md`
- `STATUS.md`

## Findings

### 1. [Medium] Potential credential/config material needs a focused secret audit

Names commonly used for credentials or sensitive tokens appear in app-owned files. Some hits may be fixtures or placeholders, but every example should be verified, documented as fake, or moved to secret management. Values are redacted here. Scanner count: 378.

Evidence:
- clawdex/share/src/app_server.rs:486 `ServerNotification::ThreadTokenUsageUpdated(_) => "thread_token_usage_updated",`
- clawdex/share/src/audit.rs:264 `if high_risk.iter().any(|token| cmd.contains(token)) {`
- clawdex/share/src/audit.rs:270 `if medium_risk.iter().any(|token| cmd.contains(token)) {`
- clawdex/share/src/audit.rs:304 `if targets.iter().any(|path| path.contains(".env") || path.contains("secrets")) {`
- clawdex/share/src/config.rs:82 `#[serde(alias = "chunkTokens")]`
- clawdex/share/src/config.rs:83 `#[serde(alias = "chunk_tokens")]`
- clawdex/share/src/config.rs:84 `pub chunk_tokens: Option<usize>,`
- clawdex/share/src/config.rs:99 `pub api_key_env: Option<String>,`
### 2. [Medium] HTML injection surfaces need sanitization review

Direct HTML insertion needs one sanitizer policy and regression tests around every untrusted content path. Scanner count: 2.

Evidence:
- clawdex/src/admin_dashboard.html:194 `document.getElementById('stats').innerHTML = html;`
- clawdex/src/admin_dashboard.html:221 `document.getElementById('pluginsBody').innerHTML = rows || '<tr><td colspan="4" class="muted">No plugins</td></tr>';`
### 3. [Medium] Runtime failure shortcuts are common enough to deserve hardening

Force unwraps, panics, unwraps, expect calls, and fatal errors should be converted to typed errors around IO, persistence, parsing, and user-driven paths. Scanner count: 159.

Evidence:
- clawdex/share/src/cron.rs:1669 `.expect("valid datetime")`
- clawdex/share/src/cron.rs:1681 `.expect("schedule spec");`
- clawdex/share/src/cron.rs:1698 `.expect("schedule spec");`
- clawdex/share/src/cron.rs:1712 `.expect("schedule spec");`
- clawdex/share/src/cron.rs:1729 `.expect("schedule spec");`
- clawdex/share/src/cron.rs:1745 `let normalized = super::normalize_job_input(&input, true).expect("normalize");`
- clawdex/share/src/cron.rs:1752 `let schedule = normalized.get("schedule").and_then(|v| v.as_object()).unwrap();`
- clawdex/share/src/cron.rs:1769 `let normalized = super::normalize_job_input(&input, true).expect("normalize");`
### 4. [Low] Large source-tree files should be checked against release strategy

Large model/media/data files can be valid, but they need clear provenance and should stay out of normal code-review diffs when possible.

Evidence:
- macClawdex/Resources/prebuilt/clawdex (51.4 MB)
- macClawdex/Resources/prebuilt/codex (50.2 MB)
- macClawdex/Resources/prebuilt/clawdexd (22.3 MB)

## Testing and Build Posture

Detected tests:
- `clawdex/tests/audit_log.rs`
- `clawdex/tests/compat_scenarios.rs`
- `clawdex/tests/memory_embeddings.rs`
- `compat/tests/fixtures/sample-cron/jobs.json`
- `compat/tests/fixtures/sample-memory/MEMORY.md`
- `compat/tests/fixtures/sample-memory/memory/2026-02-01.md`
- `compat/tests/matrix.md`

Detected CI workflows:
- `.github/workflows/clawhatch-security-self-hosted.yml`
- `.github/workflows/clawhatch-security.yml`

Inferred verification commands to standardize:
- Rust: run `cargo test` or workspace-specific checks from each Cargo workspace root.

## Review Limitations

- This was a broad static review across many local apps, not a full manual product walkthrough.
- Generated directories and dependency trees were pruned so findings focus on app-owned source.
- Secret-like values were not reproduced; examples are redacted or limited to path/line evidence.
- Pattern scanning is capped per app to keep the cross-repository sweep tractable; high-risk folders need focused follow-up review.

## Recommended Next Steps

1. Resolve every High finding first, especially secret material, tracked generated output, and dynamic execution paths.
2. Add or tighten the app's canonical CI workflow so build and tests run on every push.
3. Convert inferred build/test commands into documented commands in the app README or STATUS file.
4. Add smoke tests around app launch, persistence, API boundaries, and security-sensitive adapters.
5. Re-run this review after cleanup and replace this file with a human-reviewed release checklist.
