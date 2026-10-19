//! Turns off the Job component of iTerm's tab titles.
//!
//! Under tabterm, iTerm's job name is always tabterm itself, so the Job component only ever shows
//! `zsh (tabterm)`. See `docs/superpowers/specs/2026-10-19-iterm-title-job-design.md`.

use async_trait::async_trait;
use serde::{
    Deserialize,
    Serialize,
};
use tab_settings::{
    State,
    ValueStore,
};
use tracing::{
    info,
    warn,
};

use crate::{
    Error,
    Result,
};

pub mod api;
pub mod prefs;

/// `Title Components` bits, from iTerm's `iTermTitleComponents`.
pub const SESSION_NAME: u64 = 1;
pub const JOB: u64 = 2;

pub const TITLE_RECORD_KEY: &str = "iterm.titleJob";
pub const API_RECORD_KEY: &str = "iterm.enabledApiServer";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    pub default_guid: String,
    /// `None` when the profile has no saved value, which iTerm treats as Job only.
    pub title_components: Option<u64>,
    pub api_server_enabled: bool,
}

/// What Tab did to the title, so uninstall undoes only that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleRecord {
    pub cleared: bool,
    pub guid: String,
    pub before: u64,
    pub after: u64,
}

#[async_trait]
pub trait ItermPort: Send + Sync {
    fn is_running(&self) -> bool;
    /// `None` when iTerm has no preferences or no default profile yet.
    fn read_prefs(&self) -> Result<Option<Prefs>>;
    /// Outer `None`: no profile with that GUID. Inner `None`: the profile has no saved value.
    fn read_title_components(&self, guid: &str) -> Result<Option<Option<u64>>>;
    async fn set_title_components_live(&self, guid: &str, value: u64) -> Result<()>;
    fn write_title_components(&self, guid: &str, value: u64) -> Result<()>;
    fn write_api_server(&self, enabled: bool) -> Result<()>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum UndoOutcome {
    Nothing,
    Done,
    RestartIterm,
}

/// The real iTerm on this Mac.
pub struct SystemIterm;

#[async_trait]
impl ItermPort for SystemIterm {
    fn is_running(&self) -> bool {
        !macos_utils::applications::running_applications_matching(prefs::DOMAIN).is_empty()
    }

    fn read_prefs(&self) -> Result<Option<Prefs>> {
        Ok(prefs::export()?.as_ref().and_then(prefs::parse_prefs))
    }

    fn read_title_components(&self, guid: &str) -> Result<Option<Option<u64>>> {
        Ok(prefs::export()?.and_then(|root| prefs::title_components_for(&root, guid)))
    }

    async fn set_title_components_live(&self, guid: &str, value: u64) -> Result<()> {
        api::set_profile_property(guid, "Title Components", &value.to_string()).await
    }

    fn write_title_components(&self, guid: &str, value: u64) -> Result<()> {
        let mut root = prefs::export()?.ok_or(Error::Custom("iTerm has no preferences".into()))?;
        if !prefs::set_title_components(&mut root, guid, value) {
            return Err(Error::Custom("iTerm profile not found".into()));
        }
        prefs::import(&root)
    }

    fn write_api_server(&self, enabled: bool) -> Result<()> {
        prefs::write_api_server(enabled)
    }
}

/// Clears Job, falling back to Session Name rather than leaving the title empty.
pub fn without_job(value: u64) -> u64 {
    match value & !JOB {
        0 => SESSION_NAME,
        rest => rest,
    }
}

/// Turns Job off once. Retries on later launches until it can do so safely.
pub async fn apply(state: &State, port: &dyn ItermPort) {
    if matches!(state.get_value(TITLE_RECORD_KEY), Ok(Some(_))) {
        return;
    }
    let prefs = match port.read_prefs() {
        Ok(Some(prefs)) => prefs,
        Ok(None) => return,
        Err(err) => return warn!(%err, "iterm: could not read preferences"),
    };
    let current = prefs.title_components.unwrap_or(JOB);
    if current & JOB == 0 {
        set_record(state, &TitleRecord {
            cleared: false,
            guid: String::new(),
            before: current,
            after: current,
        });
        return;
    }
    let target = without_job(current);
    let guid = prefs.default_guid;

    if !prefs.api_server_enabled {
        match port.write_api_server(true) {
            Ok(()) => {
                info!("iterm: enabled the Python API");
                if let Err(err) = state.set_value(API_RECORD_KEY, true) {
                    warn!(%err, "iterm: could not record enabling the API");
                }
            },
            Err(err) => warn!(%err, "iterm: could not enable the Python API"),
        }
        // iTerm reads the API switch at launch, so a running iTerm can't take the live change yet.
        if port.is_running() {
            return;
        }
    }

    let written = if port.is_running() {
        port.set_title_components_live(&guid, target).await
    } else {
        port.write_title_components(&guid, target)
    };
    match written {
        Ok(()) => {
            info!(%guid, current, target, "iterm: turned off the Job title component");
            set_record(state, &TitleRecord {
                cleared: true,
                guid,
                before: current,
                after: target,
            });
        },
        Err(err) => warn!(%err, "iterm: could not turn off the Job title component; will retry"),
    }
}

/// Undoes what [`apply`] did, and nothing else.
pub async fn undo(state: &State, port: &dyn ItermPort) -> UndoOutcome {
    let mut outcome = UndoOutcome::Nothing;

    if let Ok(Some(record)) = state.get::<TitleRecord>(TITLE_RECORD_KEY) {
        if record.cleared {
            if let Ok(Some(now)) = port.read_title_components(&record.guid) {
                let now = now.unwrap_or(JOB);
                let restore = if now == record.after { record.before } else { now | JOB };
                if restore != now {
                    outcome = restore_title(port, &record.guid, restore).await;
                }
            }
        }
    }

    if state.get_bool_or(API_RECORD_KEY, false) {
        if let Err(err) = port.write_api_server(false) {
            warn!(%err, "iterm: could not disable the Python API");
        }
    }

    for key in [TITLE_RECORD_KEY, API_RECORD_KEY] {
        if let Err(err) = state.remove_value(key) {
            warn!(%err, key, "iterm: could not remove record");
        }
    }
    outcome
}

async fn restore_title(port: &dyn ItermPort, guid: &str, value: u64) -> UndoOutcome {
    let running = port.is_running();
    if running && port.set_title_components_live(guid, value).await.is_ok() {
        return UndoOutcome::Done;
    }
    match port.write_title_components(guid, value) {
        Ok(()) if running => UndoOutcome::RestartIterm,
        Ok(()) => UndoOutcome::Done,
        Err(err) => {
            warn!(%err, "iterm: could not restore the Job title component");
            UndoOutcome::Nothing
        },
    }
}

fn set_record(state: &State, record: &TitleRecord) {
    let saved = serde_json::to_value(record)
        .map_err(|err| err.to_string())
        .and_then(|value| state.set_value(TITLE_RECORD_KEY, value).map_err(|err| err.to_string()));
    if let Err(err) = saved {
        warn!(%err, "iterm: could not save record");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tab_settings::{
        State,
        ValueStore,
    };

    use super::*;

    const G: &str = "GUID";

    #[derive(Default)]
    struct FakePort {
        running: bool,
        prefs: Option<Prefs>,
        profiles: HashMap<String, Option<u64>>,
        live_fails: bool,
        calls: Mutex<Vec<String>>,
    }

    impl FakePort {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn log(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[async_trait::async_trait]
    impl ItermPort for FakePort {
        fn is_running(&self) -> bool {
            self.running
        }

        fn read_prefs(&self) -> Result<Option<Prefs>> {
            Ok(self.prefs.clone())
        }

        fn read_title_components(&self, guid: &str) -> Result<Option<Option<u64>>> {
            Ok(self.profiles.get(guid).copied())
        }

        async fn set_title_components_live(&self, guid: &str, value: u64) -> Result<()> {
            self.log(format!("live:{guid}={value}"));
            if self.live_fails {
                Err(crate::Error::Custom("live failed".into()))
            } else {
                Ok(())
            }
        }

        fn write_title_components(&self, guid: &str, value: u64) -> Result<()> {
            self.log(format!("write:{guid}={value}"));
            Ok(())
        }

        fn write_api_server(&self, enabled: bool) -> Result<()> {
            self.log(format!("api:{enabled}"));
            Ok(())
        }
    }

    fn prefs(title_components: Option<u64>, api_server_enabled: bool) -> Option<Prefs> {
        Some(Prefs {
            default_guid: G.into(),
            title_components,
            api_server_enabled,
        })
    }

    fn port(running: bool, prefs: Option<Prefs>) -> FakePort {
        FakePort {
            running,
            prefs,
            ..Default::default()
        }
    }

    fn record(state: &State) -> Option<TitleRecord> {
        state.get(TITLE_RECORD_KEY).unwrap()
    }

    fn cleared(before: u64, after: u64) -> TitleRecord {
        TitleRecord {
            cleared: true,
            guid: G.into(),
            before,
            after,
        }
    }

    fn state_with_record(record: &TitleRecord) -> State {
        let state = State::new_fake();
        state
            .set_value(TITLE_RECORD_KEY, serde_json::to_value(record).unwrap())
            .unwrap();
        state
    }

    fn undo_port(running: bool, now: u64) -> FakePort {
        let mut port = port(running, None);
        port.profiles.insert(G.into(), Some(now));
        port
    }

    #[test]
    fn without_job_clears_job_bit() {
        assert_eq!(without_job(3), 1);
        assert_eq!(without_job(2), SESSION_NAME);
        assert_eq!(without_job(2 | 4), 4);
    }

    #[tokio::test]
    async fn apply_running_uses_live_and_records() {
        let state = State::new_fake();
        let port = port(true, prefs(Some(3), true));
        apply(&state, &port).await;
        assert_eq!(port.calls(), vec!["live:GUID=1"]);
        assert_eq!(record(&state), Some(cleared(3, 1)));
    }

    #[tokio::test]
    async fn apply_closed_writes_prefs() {
        let state = State::new_fake();
        let port = port(false, prefs(Some(3), true));
        apply(&state, &port).await;
        assert_eq!(port.calls(), vec!["write:GUID=1"]);
        assert_eq!(record(&state), Some(cleared(3, 1)));
    }

    #[tokio::test]
    async fn apply_missing_key_sets_session_name() {
        let state = State::new_fake();
        let port = port(true, prefs(None, true));
        apply(&state, &port).await;
        assert_eq!(port.calls(), vec!["live:GUID=1"]);
        assert_eq!(record(&state), Some(cleared(2, 1)));
    }

    #[tokio::test]
    async fn apply_job_already_off_records_nothing_changed() {
        for value in [1, 16] {
            let state = State::new_fake();
            let port = port(true, prefs(Some(value), true));
            apply(&state, &port).await;
            assert!(port.calls().is_empty());
            assert!(!record(&state).unwrap().cleared);
        }
    }

    #[tokio::test]
    async fn apply_live_failure_writes_nothing() {
        let state = State::new_fake();
        let mut port = port(true, prefs(Some(3), true));
        port.live_fails = true;
        apply(&state, &port).await;
        assert_eq!(port.calls(), vec!["live:GUID=1"]);
        assert_eq!(record(&state), None);
    }

    #[tokio::test]
    async fn apply_api_off_while_running_enables_api_only() {
        let state = State::new_fake();
        let port = port(true, prefs(Some(3), false));
        apply(&state, &port).await;
        assert_eq!(port.calls(), vec!["api:true"]);
        assert_eq!(state.get_bool(API_RECORD_KEY).unwrap(), Some(true));
        assert_eq!(record(&state), None);
    }

    #[tokio::test]
    async fn apply_api_off_while_closed_enables_api_and_writes() {
        let state = State::new_fake();
        let port = port(false, prefs(Some(3), false));
        apply(&state, &port).await;
        assert_eq!(port.calls(), vec!["api:true", "write:GUID=1"]);
        assert_eq!(state.get_bool(API_RECORD_KEY).unwrap(), Some(true));
        assert_eq!(record(&state), Some(cleared(3, 1)));
    }

    #[tokio::test]
    async fn apply_no_iterm_prefs_records_nothing() {
        let state = State::new_fake();
        let port = port(true, None);
        apply(&state, &port).await;
        assert!(port.calls().is_empty());
        assert_eq!(record(&state), None);
    }

    #[tokio::test]
    async fn apply_runs_once() {
        let state = state_with_record(&cleared(3, 1));
        let port = port(true, prefs(Some(3), true));
        apply(&state, &port).await;
        assert!(port.calls().is_empty());
    }

    #[tokio::test]
    async fn undo_restores_exact_original_when_unchanged() {
        let state = state_with_record(&cleared(3, 1));
        let port = undo_port(true, 1);
        assert_eq!(undo(&state, &port).await, UndoOutcome::Done);
        assert_eq!(port.calls(), vec!["live:GUID=3"]);
        assert_eq!(record(&state), None);
    }

    #[tokio::test]
    async fn undo_keeps_user_bits_when_changed() {
        let state = state_with_record(&cleared(3, 1));
        let port = undo_port(true, 1 | 4);
        undo(&state, &port).await;
        assert_eq!(port.calls(), vec!["live:GUID=7"]);
    }

    #[tokio::test]
    async fn undo_closed_writes_prefs() {
        let state = state_with_record(&cleared(3, 1));
        let port = undo_port(false, 1);
        assert_eq!(undo(&state, &port).await, UndoOutcome::Done);
        assert_eq!(port.calls(), vec!["write:GUID=3"]);
    }

    #[tokio::test]
    async fn undo_live_failure_falls_back_and_asks_restart() {
        let state = state_with_record(&cleared(3, 1));
        let mut port = undo_port(true, 1);
        port.live_fails = true;
        assert_eq!(undo(&state, &port).await, UndoOutcome::RestartIterm);
        assert_eq!(port.calls(), vec!["live:GUID=3", "write:GUID=3"]);
    }

    #[tokio::test]
    async fn undo_profile_gone_leaves_iterm_alone() {
        let state = state_with_record(&cleared(3, 1));
        let port = port(true, None);
        assert_eq!(undo(&state, &port).await, UndoOutcome::Nothing);
        assert!(port.calls().is_empty());
        assert_eq!(record(&state), None);
    }

    #[tokio::test]
    async fn undo_disables_api_only_if_tab_enabled_it() {
        let state = state_with_record(&cleared(3, 1));
        state.set_value(API_RECORD_KEY, true).unwrap();
        let port = undo_port(true, 1);
        undo(&state, &port).await;
        assert_eq!(port.calls().last().unwrap(), "api:false");
        assert_eq!(state.get_bool(API_RECORD_KEY).unwrap(), None);

        let state = state_with_record(&cleared(3, 1));
        let port = undo_port(true, 1);
        undo(&state, &port).await;
        assert!(!port.calls().iter().any(|call| call.starts_with("api:")));
    }

    #[tokio::test]
    async fn undo_nothing_cleared_does_nothing() {
        let state = state_with_record(&TitleRecord {
            cleared: false,
            guid: String::new(),
            before: 1,
            after: 1,
        });
        let port = undo_port(true, 1);
        assert_eq!(undo(&state, &port).await, UndoOutcome::Nothing);
        assert!(port.calls().is_empty());
    }
}
