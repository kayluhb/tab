# iTerm Title Job Component Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tab turns off the Job component of iTerm's default-profile tab title once (live via iTerm's API, else via preferences while iTerm is closed), and undoes exactly its own changes on uninstall.

**Architecture:** A new macOS-only module `tab_integrations::iterm`. Decision logic (`apply` / `undo`) is pure over an `ItermPort` trait and Tab's `State`, so it is unit-tested with a fake. Real I/O lives behind that trait: preference read/write through `defaults export|import|write`, and a minimal iTerm API client (websocket over iTerm's unix socket, hand-written `prost` messages, cookie from AppleScript). `run_install` calls `apply`; `uninstall_terminal_integrations` calls `undo`.

**Tech Stack:** Rust, tokio, tokio-tungstenite 0.26 (workspace), prost 0.13.5, plist 1.7, async-trait, `tab_settings::State`.

**Spec:** `docs/superpowers/specs/2026-10-19-iterm-title-job-design.md`

## Global Constraints

- Only iTerm's default profile (the `New Bookmarks` entry whose `Guid` equals `Default Bookmark Guid`), only its `Title Components` value, and the global `EnableAPIServer` — nothing else in `com.googlecode.iterm2`.
- `Title Components` bits: Session Name = `1`, Job = `2`. A profile without the key means iTerm's default, **Job only (`2`)** (iTerm `iTermProfilePreferences.m:1278`).
- Never send AppleScript to iTerm unless it is already running (it would launch iTerm).
- Never block Tab's launch: the whole live (A) attempt is bounded by a 5-second timeout; any failure is logged and treated as "A failed".
- Do not vendor iTerm's `api.proto` (GPL-2.0); hand-write the `prost` structs.
- State keys: `iterm.titleJob` (JSON record) and `iterm.enabledApiServer` (bool).
- The module compiles only on macOS: `#[cfg(target_os = "macos")] pub mod iterm;`.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. The pre-commit hook runs `cargo clippy --locked -D warnings`; after adding dependencies, run `cargo build -p tab_integrations` once so `Cargo.lock` is updated, and commit the lock.

## Review Focus

- Profile with no `Title Components` key (Job-only default): clearing Job must not leave `0` (empty title); write Session Name (`1`). Pinned in Task 1.
- User edits the title after Tab's change, then uninstalls: undo adds the Job bit back and keeps their other bits; it restores the exact original only if the value is still what Tab wrote. Pinned in Task 1.
- Default profile changed or deleted between apply and undo: undo targets the recorded GUID; if that profile no longer exists it does nothing to iTerm and still clears the record. Pinned in Task 1.
- iTerm's API hangs or the socket is missing while iTerm runs: no record is written, nothing is written via preferences, next launch retries. Pinned in Task 1 (port error) and Task 3 (timeout wraps the client).
- `Custom` title (bit `16`, mutually exclusive with others) or Job already off: record "changed nothing", never touch. Pinned in Task 1.

---

### Task 1: Decision logic (`apply` / `undo`) over a port

**Files:**
- Create: `crates/tab_integrations/src/iterm/mod.rs`
- Modify: `crates/tab_integrations/src/lib.rs` (add `#[cfg(target_os = "macos")] pub mod iterm;`)

**Interfaces:**
- Produces (all `pub` in `tab_integrations::iterm`):
  - `pub const SESSION_NAME: u64 = 1; pub const JOB: u64 = 2;`
  - `pub const TITLE_RECORD_KEY: &str = "iterm.titleJob"; pub const API_RECORD_KEY: &str = "iterm.enabledApiServer";`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Prefs { pub default_guid: String, pub title_components: Option<u64>, pub api_server_enabled: bool }`
  - `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] pub struct TitleRecord { pub cleared: bool, pub guid: String, pub before: u64, pub after: u64 }` (`guid`/`before`/`after` meaningful only when `cleared`)
  - `#[async_trait] pub trait ItermPort: Send + Sync { fn is_running(&self) -> bool; fn read_prefs(&self) -> Result<Option<Prefs>>; fn read_title_components(&self, guid: &str) -> Result<Option<Option<u64>>>; async fn set_title_components_live(&self, guid: &str, value: u64) -> Result<()>; fn write_title_components(&self, guid: &str, value: u64) -> Result<()>; fn write_api_server(&self, enabled: bool) -> Result<()>; }`
    - `read_prefs`: `Ok(None)` when iTerm has no preference domain or no default profile.
    - `read_title_components`: outer `None` = profile with that GUID doesn't exist; inner `None` = key absent.
  - `pub fn without_job(value: u64) -> u64` — clears `JOB`; if the result is `0`, returns `SESSION_NAME`.
  - `#[derive(Debug, PartialEq, Eq)] pub enum UndoOutcome { Nothing, Done, RestartIterm }`
  - `pub async fn apply(state: &State, port: &dyn ItermPort)` — logs errors, never returns them.
  - `pub async fn undo(state: &State, port: &dyn ItermPort) -> UndoOutcome`
- `Result` is `crate::Result`; add an `Error::Iterm(String)` variant in `crates/tab_integrations/src/error.rs` if no existing variant fits.

**Algorithm (the spec's apply/undo, made exact):**

```
apply:
  if state has TITLE_RECORD_KEY: return
  prefs = port.read_prefs()  (Err or None: return, no record)
  current = prefs.title_components.unwrap_or(JOB)
  if current & JOB == 0: record {cleared:false, guid:"", before:current, after:current}; return
  target = without_job(current)
  if !prefs.api_server_enabled:
      port.write_api_server(true) (Ok -> state API_RECORD_KEY = true)
      if port.is_running(): return            // API needs an iTerm restart
  if port.is_running():
      if port.set_title_components_live(guid, target) fails: return   // retry next launch
  else:
      if port.write_title_components(guid, target) fails: return
  record {cleared:true, guid, before:current, after:target}

undo:
  outcome = Nothing
  if record.cleared:
      match port.read_title_components(record.guid):
        Ok(Some(now)):
          now = now.unwrap_or(JOB)
          restore = if now == record.after { record.before } else { now | JOB }
          if restore != now:
              live = port.is_running() && set_title_components_live(guid, restore).is_ok()
              if !live: write_title_components(guid, restore); outcome = if is_running {RestartIterm} else {Done}
              else outcome = Done
        _ => {}   // profile gone or unreadable: leave iTerm alone
  if state API_RECORD_KEY == true: port.write_api_server(false)
  remove both keys
  return outcome
```

- [ ] **Step 1: Write the failing tests** (inline `#[cfg(test)] mod tests` in `mod.rs`, `#[tokio::test]`, using `State::new_fake()` and a `FakePort` with fields `running: bool, prefs: Option<Prefs>, profiles: HashMap<String, Option<u64>>, live_fails: bool, calls: Mutex<Vec<String>>`; each port method pushes e.g. `"live:GUID=1"`, `"write:GUID=1"`, `"api:true"`)

```rust
#[test] fn without_job_clears_job_bit() { assert_eq!(without_job(3), 1); assert_eq!(without_job(2), SESSION_NAME); assert_eq!(without_job(2 | 4), 4); }
// apply
#[tokio::test] async fn apply_running_uses_live_and_records()      // prefs {G, Some(3), api:true}, running -> calls ["live:G=1"], record {cleared:true, G, before:3, after:1}
#[tokio::test] async fn apply_closed_writes_prefs()                 // running:false -> calls ["write:G=1"], record cleared:true
#[tokio::test] async fn apply_missing_key_sets_session_name()       // title_components None -> "live:G=1", before:2
#[tokio::test] async fn apply_job_already_off_records_nothing_changed() // Some(1) and Some(16) -> no calls, record cleared:false
#[tokio::test] async fn apply_live_failure_writes_nothing()         // live_fails -> calls ["live:G=1"], no record
#[tokio::test] async fn apply_api_off_while_running_enables_api_only() // api:false, running -> ["api:true"], API_RECORD_KEY == true, no title record
#[tokio::test] async fn apply_api_off_while_closed_enables_api_and_writes() // api:false, closed -> ["api:true","write:G=1"], both records
#[tokio::test] async fn apply_no_iterm_prefs_records_nothing()      // prefs None -> no calls, no record
#[tokio::test] async fn apply_runs_once()                           // existing record -> no calls
// undo
#[tokio::test] async fn undo_restores_exact_original_when_unchanged() // record{G,before:3,after:1}, profile G=Some(1), running -> ["live:G=3"], Done, keys removed
#[tokio::test] async fn undo_keeps_user_bits_when_changed()          // profile G=Some(1|4) -> "live:G=7"
#[tokio::test] async fn undo_closed_writes_prefs()                   // running:false -> ["write:G=3"], Done
#[tokio::test] async fn undo_live_failure_falls_back_and_asks_restart() // live_fails, running -> ["live:G=3","write:G=3"], RestartIterm
#[tokio::test] async fn undo_profile_gone_leaves_iterm_alone()       // profiles has no G -> no title calls, keys removed, Nothing
#[tokio::test] async fn undo_disables_api_only_if_tab_enabled_it()   // with API_RECORD_KEY -> last call "api:false"; without -> no "api:" call
#[tokio::test] async fn undo_nothing_cleared_does_nothing()          // record cleared:false -> no calls, Nothing
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tab_integrations iterm::`
Expected: compile errors (items not defined).

- [ ] **Step 3: Implement the items in the Interfaces block in `crates/tab_integrations/src/iterm/mod.rs`** following the algorithm above. Log with `tracing` (`info!` on each write, `warn!` on failures).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tab_integrations iterm::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/tab_integrations/src/iterm/mod.rs crates/tab_integrations/src/lib.rs crates/tab_integrations/src/error.rs
git commit -m "iterm: decide when to turn off the Job title component, and undo it"
```

---

### Task 2: Reading and writing iTerm's preferences

**Files:**
- Create: `crates/tab_integrations/src/iterm/prefs.rs`
- Modify: `crates/tab_integrations/src/iterm/mod.rs` (`mod prefs;`)

**Interfaces:**
- Consumes: `Prefs` from Task 1.
- Produces:
  - `pub const DOMAIN: &str = "com.googlecode.iterm2";`
  - `pub fn parse_prefs(root: &plist::Value) -> Option<Prefs>` — `Default Bookmark Guid`, the matching `New Bookmarks[i]."Title Components"` (integer → `u64`), `EnableAPIServer` (absent → `false`).
  - `pub fn title_components_for(root: &plist::Value, guid: &str) -> Option<Option<u64>>`
  - `pub fn set_title_components(root: &mut plist::Value, guid: &str, value: u64) -> bool` — `false` if no profile has that GUID; inserts the key if absent; touches nothing else.
  - `pub fn export() -> Result<Option<plist::Value>>` — `defaults export com.googlecode.iterm2 -`; non-zero exit (domain missing) → `Ok(None)`.
  - `pub fn import(root: &plist::Value) -> Result<()>` — serialize as XML and pipe to `defaults import com.googlecode.iterm2 -`.
  - `pub fn write_api_server(enabled: bool) -> Result<()>` — `defaults write com.googlecode.iterm2 EnableAPIServer -bool true|false`.

- [ ] **Step 1: Write the failing tests** (inline; build fixtures with `plist::Dictionary`)

```rust
#[test] fn parse_reads_default_profile()          // two profiles, default = second (TC 3), EnableAPIServer true -> Prefs{guid2, Some(3), true}
#[test] fn parse_missing_key_is_none()            // default profile without "Title Components" -> title_components None
#[test] fn parse_without_default_guid_is_none()   // no "Default Bookmark Guid" -> None
#[test] fn parse_api_server_absent_is_false()
#[test] fn set_changes_only_that_profile()        // set guid2 = 1 -> guid2 TC 1, guid1 unchanged, other keys of guid2 unchanged
#[test] fn set_inserts_missing_key()
#[test] fn set_unknown_guid_returns_false()
#[test] fn title_components_for_unknown_guid_is_none()
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tab_integrations iterm::prefs` → compile errors.

- [ ] **Step 3: Implement the functions above.** Shell out with `std::process::Command`; `import` writes the XML to the child's stdin.

- [ ] **Step 4: Run to verify pass** — `cargo test -p tab_integrations iterm::prefs` → all pass. Then a read-only check against the real machine: add a temporary `#[ignore]` test (or `cargo run` snippet) that prints `parse_prefs(&export()?.unwrap())`; expected on the author's Mac: `Prefs { default_guid: "B69E62C4-AE81-40A8-A528-E10F47532AD3", title_components: Some(3), api_server_enabled: true }`. Remove it before committing.

- [ ] **Step 5: Commit**

```bash
git add crates/tab_integrations/src/iterm/prefs.rs crates/tab_integrations/src/iterm/mod.rs
git commit -m "iterm: read and write the default profile's title components in iTerm's preferences"
```

---

### Task 3: Minimal iTerm API client

**Files:**
- Create: `crates/tab_integrations/src/iterm/api.rs`
- Modify: `crates/tab_integrations/src/iterm/mod.rs` (`mod api;`), `crates/tab_integrations/Cargo.toml` (macOS deps: `prost = "0.13.5"`, `tokio-tungstenite.workspace = true`, `http.workspace = true`), `Cargo.lock`

**Interfaces:**
- Produces:
  - `pub async fn set_profile_property(guid: &str, key: &str, json_value: &str) -> Result<()>` — whole call wrapped in `tokio::time::timeout(Duration::from_secs(5), …)`.
  - Internal `prost` messages (proto2 semantics → every scalar `optional`):
    - `ClientOriginatedMessage { id: Option<i64> = 1, set_profile_property_request: Option<SetProfilePropertyRequest> = 105 }`
    - `SetProfilePropertyRequest { guid_list: Option<GuidList> = 2, key: Option<String> = 3, json_value: Option<String> = 4 }`
    - `GuidList { guids: Vec<String> = 1 }`
    - `ServerOriginatedMessage { id: Option<i64> = 1, error: Option<String> = 2, set_profile_property_response: Option<SetProfilePropertyResponse> = 105 }`
    - `SetProfilePropertyResponse { status: Option<i32> = 1 }` — `0` = OK (`1` SESSION_NOT_FOUND, `2` REQUEST_MALFORMED, `3` BAD_GUID)
- Protocol (from the spec): cookie and key from `osascript -e 'tell application "iTerm2" to request cookie and key for app named "Tab"'` → stdout `"<cookie> <key>"`. Connect `tokio::net::UnixStream` to `~/Library/Application Support/iTerm2/private/socket`; `tokio_tungstenite::client_async` with request URI `ws://localhost/` and headers `origin: ws://localhost/`, `sec-websocket-protocol: api.iterm2.com`, `x-iterm2-cookie`, `x-iterm2-key`, `x-iterm2-advisory-name: Tab`, `x-iterm2-disable-auth-ui: true`, `x-iterm2-library-version: tab 1`. Send one binary frame, read binary frames until one with the same `id`. Success iff `error` is `None` and `status == Some(0)`.

- [ ] **Step 1: Write the failing tests** (byte vectors generated with iTerm's official Python bindings)

```rust
#[test] fn encodes_set_profile_property_like_iterm() {
    // id 7, guid B69E62C4-AE81-40A8-A528-E10F47532AD3, key "Title Components", json "1"
    assert_eq!(hex(&request(7, "B69E62C4-AE81-40A8-A528-E10F47532AD3", "Title Components", "1").encode_to_vec()),
        "0807ca063d12260a2442363945363243342d414538312d343041382d413532382d4531304634373533324144331a105469746c6520436f6d706f6e656e7473220131");
}
#[test] fn decodes_ok_response()       { /* "0807ca06020800" -> id Some(7), status Some(0) -> check_response(7, ..) is Ok */ }
#[test] fn decodes_bad_guid_response() { /* "0807ca06020803" -> check_response(7, ..) is Err */ }
```

where `fn request(id: i64, guid: &str, key: &str, json_value: &str) -> ClientOriginatedMessage` and `fn check_response(id: i64, msg: &ServerOriginatedMessage) -> Result<()>` are private helpers in `api.rs`.

- [ ] **Step 2: Run to verify failure** — `cargo test -p tab_integrations iterm::api` → compile errors.

- [ ] **Step 3: Add the dependencies, then implement the messages, `request`, `check_response`, and `set_profile_property`.** Run `cargo build -p tab_integrations` so `Cargo.lock` updates.

- [ ] **Step 4: Run to verify pass** — `cargo test -p tab_integrations iterm::api` → pass. Then `cargo clippy --locked -p tab_integrations -- -D warnings` → clean.

- [ ] **Step 5: Commit**

```bash
git add crates/tab_integrations/src/iterm/api.rs crates/tab_integrations/src/iterm/mod.rs crates/tab_integrations/Cargo.toml Cargo.lock
git commit -m "iterm: minimal client for iTerm's API to set a profile property"
```

---

### Task 4: Real port, wiring, Info.plist, and hand verification

**Files:**
- Modify: `crates/tab_integrations/src/iterm/mod.rs` (add `SystemIterm`)
- Modify: `crates/tab_desktop/src/install.rs:47-80` (`run_install`)
- Modify: `crates/tab_install/src/macos.rs:82-96` (`uninstall_terminal_integrations`)
- Modify: `build-scripts/build.py:354` (after the `LSUIElement` write)

**Interfaces:**
- Consumes: Task 1's `ItermPort`, `apply`, `undo`, `UndoOutcome`; Task 2's `prefs::*`; Task 3's `api::set_profile_property`.
- Produces: `pub struct SystemIterm;` implementing `ItermPort`:
  - `is_running` → `!macos_utils::applications::running_applications_matching("com.googlecode.iterm2").is_empty()`
  - `read_prefs` / `read_title_components` → `prefs::export()` + `parse_prefs` / `title_components_for`
  - `set_title_components_live` → `api::set_profile_property(guid, "Title Components", &value.to_string())`
  - `write_title_components` → export, `set_title_components`, import
  - `write_api_server` → `prefs::write_api_server`

- [ ] **Step 1: Wire apply.** In `run_install`, after the VSCode block: `tab_integrations::iterm::apply(&tab_settings::State::new(), &tab_integrations::iterm::SystemIterm).await;`

- [ ] **Step 2: Wire undo.** In `uninstall_terminal_integrations`, right after the iTerm AutoLaunch removal loop: call `undo(&State::new(), &SystemIterm).await`; on `UndoOutcome::RestartIterm` print to stderr: `Restart iTerm to bring back the job name in tab titles.`

- [ ] **Step 3: Add the usage string.** In `build-scripts/build.py`, after the `LSUIElement` line:
  `run_cmd(["defaults", "write", info_plist_path, "NSAppleEventsUsageDescription", "Tab connects to iTerm to stop tab titles showing its terminal wrapper instead of the running command."])`

- [ ] **Step 4: Build and check** — `cargo build -p tab_desktop -p tab_install` succeeds; `cargo clippy --locked --workspace -- -D warnings` clean; `cargo test -p tab_integrations iterm` passes.

- [ ] **Step 5: Verify by hand on the author's Mac (iTerm running, API already on via corral).** Back up first: `defaults export com.googlecode.iterm2 ~/iterm2-before-tab.plist`.
  1. `scripts/build-local.sh`, then `plutil -extract NSAppleEventsUsageDescription raw build/Tab.app/Contents/Info.plist` prints the string.
  2. Launch the built Tab.app. Accept macOS's "control iTerm2" prompt and iTerm's API prompt.
  3. Open a new iTerm tab and run `claude`: the title no longer contains `('zsh (tabterm)')`. `defaults export com.googlecode.iterm2 - | plutil -extract "New Bookmarks.0.Title Components" raw -` prints `1`.
  4. `sqlite3 ~/Library/Application\ Support/tab/data.sqlite3 "select * from state where key like 'iterm.%'"` shows the `iterm.titleJob` record with `cleared: true`. (Confirm the data-dir path and table name in `crates/tab_settings/src/sqlite/mod.rs` before running.)
  5. Quit and relaunch Tab: no further iTerm writes (log shows no `iterm` write lines).
  6. Undo, without running a full uninstall: add a temporary `#[ignore]` test that calls `undo(&State::new(), &SystemIterm)` and run it, or run `tab uninstall` on a throwaway user account. Afterwards the value is back to `3` and the record is gone.

- [ ] **Step 6: Commit**

```bash
git add crates/tab_integrations/src/iterm/mod.rs crates/tab_desktop/src/install.rs crates/tab_install/src/macos.rs build-scripts/build.py
git commit -m "Turn off iTerm's Job title component once at launch; undo on uninstall"
```
