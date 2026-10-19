# Turn off the "Job" part of iTerm tab titles

## Why

Inside iTerm, Tab wraps every shell in `tabterm`, which runs the real shell on its own
PTY. iTerm picks the job name for a tab title only from processes attached to the tab's
own tty (`deepestForegroundJob(attachedToTTYRdev:)` in iTerm's
`sources/ProcessInfo/iTermProcessInfo.swift`). Everything the user runs — `claude`,
`vim`, … — is on tabterm's inner tty, so iTerm always falls back to tabterm itself and
every title reads `… ('zsh (tabterm)')`.

The job name can never be correct while tabterm is in the path, and dropping tabterm in
iTerm is not an option (Tab's completions need it). So Tab should switch off the Job
title component in iTerm, once, and put it back on uninstall.

## Goals

- After Tab is set up, new iTerm tabs no longer show `(zsh (tabterm))` in their titles.
- Done once, without a prompt (same footing as Tab editing `.zshrc`).
- Uninstalling Tab turns Job back on if, and only if, Tab turned it off.
- Works for the author today, and is polite enough to ship to other users later.

## Non-goals

- Making iTerm show the real job name (would need iTerm changes or tabterm process-title
  tricks; rejected).
- Changing any profile other than iTerm's default profile, any title component other
  than Job, or any other iTerm setting except `EnableAPIServer` (see below).
- Other terminals.

## Anti-goals (would be a failure even if it works)

- Re-applying after the user turns Job back on themselves. Once means once.
- Clobbering other title components or other profile settings.
- Turning Job on at uninstall when the user had it off before Tab.
- Blocking or failing Tab's install when iTerm is absent, closed, or refuses the API.

## Facts established

- iTerm stores the setting as the integer `Title Components` in each entry of
  `New Bookmarks` in `com.googlecode.iterm2`. It is a bitmask; Session Name = 1, Job = 2.
  The author's default profile has `3`. The default profile is the one whose `Guid` equals
  the top-level `Default Bookmark Guid`.
- Writing the value with `defaults import` while iTerm is running changes the stored
  value but **does not** affect the running iTerm (tested 2026-10-19: a new tab still
  showed the job). iTerm reads profiles at launch.
- iTerm's Python API can change a profile property live. corral does this today for the
  status bar (`corral/src/corral/iterm_setup.py`). Protocol:
  - websocket over the unix socket `~/Library/Application Support/iTerm2/private/socket`
    (TCP `ws://localhost:1912` is the fallback the Python library uses), subprotocol
    `api.iterm2.com`, headers `origin: ws://localhost/`, `x-iterm2-cookie`,
    `x-iterm2-key`, `x-iterm2-advisory-name`, `x-iterm2-disable-auth-ui: true`;
  - cookie and key come from AppleScript:
    `tell application "iTerm2" to request cookie and key for app named "Tab"`. The first
    time, iTerm shows its own "allow Tab to control iTerm?" prompt;
  - the change is one `ClientOriginatedMessage` carrying a `SetProfilePropertyRequest`
    with `guid_list` = [default GUID], `key` = `"Title Components"`,
    `json_value` = the new number;
  - the API only works when `EnableAPIServer` is on, which iTerm reads at launch.

## How it works (decided with the author)

**A, then B.**

- **A — live, via iTerm's API** when iTerm is running and the API answers.
- **B — write the preference** (`com.googlecode.iterm2`) when A is not possible. Takes
  effect on iTerm's next launch.

Apply step, run on each launch until it records success:

1. If the "applied" record exists, stop.
2. If iTerm isn't installed, or has no default profile yet, stop without recording; check
   again next launch. *(Claude's call.)*
3. Read the default profile's `Title Components`. If the Job bit is already off, record
   "applied, Tab changed nothing" and stop.
4. New value = old value with the Job bit cleared.
   - iTerm running: write with A. If A fails, write nothing and don't record; the next
     launch tries again (Tab's launch agent usually starts before iTerm at login, which
     lands in the next branch). *(Claude's call: avoids a B write that a running iTerm
     could overwrite, and that "once" would then never retry.)*
   - iTerm not running: write with B.
5. Record "applied, Tab cleared Job on profile `<GUID>`".

**iTerm's API switch.** A needs `EnableAPIServer` on, which iTerm reads at launch. The
author decided Tab turns it on itself, the way corral's `install.sh` does:

- Before step 4, if `EnableAPIServer` is off, Tab writes it on (preference write) and
  records "Tab enabled the API". With iTerm running, A still can't work until iTerm
  restarts, so that launch writes nothing to the profile and retries later, as above.
- Undo at uninstall turns `EnableAPIServer` off only if that record exists, after the
  title undo (which may need the API). Separate record from the title one.

**System prompts.** The author accepted that A shows two one-time prompts: macOS's
Automation prompt ("Tab wants to control iTerm2") and iTerm's own API permission prompt.
"Without a prompt" means Tab itself never asks. For macOS to show its prompt instead of
silently refusing (-1743), Tab.app's `Info.plist` needs `NSAppleEventsUsageDescription`
— add it where `build-scripts/build.py:347-354` writes `LSUIElement`, plus wherever
`scripts/build-local.sh` produces the local bundle. The entitlement
`com.apple.security.automation.apple-events` is already in
`crates/tab_desktop/entitlements.plist`. If the user denies either prompt, A fails and
the apply step behaves as above (retry; B once iTerm is closed).

Never send AppleScript to iTerm unless it is already running — `tell application
"iTerm2"` would launch it. *(Claude's call.)*

Undo step, at uninstall:

1. If the record says Tab cleared Job, set the Job bit on that profile again, leaving
   every other bit as it is now: A if iTerm is running and A works, else B. If B was used
   while iTerm is running, tell the user to restart iTerm (uninstall is a one-shot, so
   there is no later retry). Otherwise do nothing. *(Claude's call.)*
2. Remove the record.

## Where it lives (builder's call, with the author's constraints)

- One small module owns iTerm preference reading, the API client, and apply/undo — e.g.
  `tab_integrations::iterm`. Tab has no existing helper for reading another app's
  preferences, nor any AppleScript or websocket-client code.
- Apply runs from the desktop app's `run_install` (`crates/tab_desktop/src/install.rs:47`),
  which runs on every launch; the "applied" record (not the version check) makes it once.
- Undo runs in `uninstall_terminal_integrations()` (`crates/tab_install/src/macos.rs:82`),
  next to the existing iTerm cleanup. That runs before the data dir (and so the record)
  is deleted.
- The record goes in `tab_settings::state` (SQLite key/value, JSON values).

## Left to the builder

- The Rust websocket client: `tokio-tungstenite` 0.26 (already a workspace dep) via
  `client_async` over `tokio::net::UnixStream`; `prost` for the messages.
- How to get the protobuf types. **Do not vendor iTerm's `api.proto` without checking its
  license** — iTerm2 is GPL-2.0 and Tab is MIT/Apache. Hand-writing the few `prost`
  message structs needed (with matching field numbers) avoids the question.
- How to read the default GUID and current value: from the preference domain (works
  whether iTerm is running or not), or over the API when A is used.
- Running `osascript` as a subprocess vs. an AppleScript framework call.
- Timeouts: none of this may hang Tab's launch; a few seconds total, then fall back to B.
- Tests: pure functions (bit math, decision of apply/undo from record + current value)
  get inline unit tests, like `install.rs`. The API client and preference I/O are
  verified by hand against a real iTerm.

## Open risks

- **B may be undone by iTerm.** If iTerm writes its in-memory profiles back when it quits
  or when the user edits any profile, a B write made while iTerm was running could be
  lost. Not yet tested (needs iTerm quit). Apply avoids this by using B only when iTerm
  is closed; only undo can hit it.
- **A missing `Title Components` key** may mean iTerm's built-in default. The builder
  confirms what iTerm uses when the key is absent before treating it as a number.
- The `EnableAPIServer` write made while iTerm is running carries the same overwrite risk
  as B. corral relies on the same write and it has held for the author.
