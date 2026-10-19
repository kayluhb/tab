//! iTerm's preferences, read and written through `defaults` so the preference cache stays in sync.

use std::io::Write as _;
use std::process::{
    Command,
    Stdio,
};

use plist::Value;

use super::Prefs;
use crate::{
    Error,
    Result,
};

pub const DOMAIN: &str = "com.googlecode.iterm2";

const DEFAULT_GUID: &str = "Default Bookmark Guid";
const PROFILES: &str = "New Bookmarks";
const GUID: &str = "Guid";
const TITLE_COMPONENTS: &str = "Title Components";
const API_SERVER: &str = "EnableAPIServer";

pub fn parse_prefs(root: &Value) -> Option<Prefs> {
    let default_guid = root.as_dictionary()?.get(DEFAULT_GUID)?.as_string()?;
    Some(Prefs {
        title_components: title_components_for(root, default_guid)?,
        default_guid: default_guid.to_owned(),
        api_server_enabled: root
            .as_dictionary()?
            .get(API_SERVER)
            .and_then(Value::as_boolean)
            .unwrap_or(false),
    })
}

pub fn title_components_for(root: &Value, guid: &str) -> Option<Option<u64>> {
    let profile = profiles(root)?
        .iter()
        .filter_map(Value::as_dictionary)
        .find(|profile| profile.get(GUID).and_then(Value::as_string) == Some(guid))?;
    Some(profile.get(TITLE_COMPONENTS).and_then(Value::as_unsigned_integer))
}

pub fn set_title_components(root: &mut Value, guid: &str, value: u64) -> bool {
    let profile = root
        .as_dictionary_mut()
        .and_then(|root| root.get_mut(PROFILES))
        .and_then(Value::as_array_mut)
        .and_then(|profiles| {
            profiles
                .iter_mut()
                .filter_map(Value::as_dictionary_mut)
                .find(|profile| profile.get(GUID).and_then(Value::as_string) == Some(guid))
        });
    match profile {
        Some(profile) => {
            profile.insert(TITLE_COMPONENTS.into(), value.into());
            true
        },
        None => false,
    }
}

fn profiles(root: &Value) -> Option<&Vec<Value>> {
    root.as_dictionary()?.get(PROFILES)?.as_array()
}

/// `None` when iTerm has no preferences yet.
pub fn export() -> Result<Option<Value>> {
    let output = Command::new("defaults").args(["export", DOMAIN, "-"]).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(Value::from_reader_xml(output.stdout.as_slice())?))
}

pub fn import(root: &Value) -> Result<()> {
    let mut xml = Vec::new();
    root.to_writer_xml(&mut xml)?;
    let mut child = Command::new("defaults")
        .args(["import", DOMAIN, "-"])
        .stdin(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("stdin is piped").write_all(&xml)?;
    check(child.wait()?, "defaults import")
}

pub fn write_api_server(enabled: bool) -> Result<()> {
    let status = Command::new("defaults")
        .args([
            "write",
            DOMAIN,
            API_SERVER,
            "-bool",
            if enabled { "true" } else { "false" },
        ])
        .status()?;
    check(status, "defaults write")
}

fn check(status: std::process::ExitStatus, what: &'static str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::Custom(format!("{what} failed: {status}").into()))
    }
}

#[cfg(test)]
mod tests {
    use plist::{
        Dictionary,
        Value,
    };

    use super::*;

    fn profile(guid: &str, title_components: Option<u64>) -> Value {
        let mut dict = Dictionary::new();
        dict.insert("Guid".into(), guid.into());
        dict.insert("Name".into(), format!("profile {guid}").into());
        if let Some(value) = title_components {
            dict.insert("Title Components".into(), value.into());
        }
        Value::Dictionary(dict)
    }

    fn root(default_guid: Option<&str>, profiles: Vec<Value>, api: Option<bool>) -> Value {
        let mut dict = Dictionary::new();
        if let Some(guid) = default_guid {
            dict.insert("Default Bookmark Guid".into(), guid.into());
        }
        dict.insert("New Bookmarks".into(), Value::Array(profiles));
        if let Some(api) = api {
            dict.insert("EnableAPIServer".into(), api.into());
        }
        Value::Dictionary(dict)
    }

    fn two_profiles() -> Value {
        root(
            Some("g2"),
            vec![profile("g1", Some(3)), profile("g2", Some(3))],
            Some(true),
        )
    }

    #[test]
    fn parse_reads_default_profile() {
        assert_eq!(
            parse_prefs(&two_profiles()),
            Some(Prefs {
                default_guid: "g2".into(),
                title_components: Some(3),
                api_server_enabled: true,
            })
        );
    }

    #[test]
    fn parse_missing_key_is_none() {
        let root = root(Some("g1"), vec![profile("g1", None)], Some(true));
        assert_eq!(parse_prefs(&root).unwrap().title_components, None);
    }

    #[test]
    fn parse_without_default_guid_is_none() {
        let root = root(None, vec![profile("g1", Some(3))], Some(true));
        assert_eq!(parse_prefs(&root), None);
    }

    #[test]
    fn parse_without_default_profile_is_none() {
        let root = root(Some("missing"), vec![profile("g1", Some(3))], Some(true));
        assert_eq!(parse_prefs(&root), None);
    }

    #[test]
    fn parse_api_server_absent_is_false() {
        let root = root(Some("g1"), vec![profile("g1", Some(3))], None);
        assert!(!parse_prefs(&root).unwrap().api_server_enabled);
    }

    #[test]
    fn set_changes_only_that_profile() {
        let mut root = two_profiles();
        assert!(set_title_components(&mut root, "g2", 1));
        assert_eq!(title_components_for(&root, "g2"), Some(Some(1)));
        assert_eq!(title_components_for(&root, "g1"), Some(Some(3)));
        let profiles = root.as_dictionary().unwrap()["New Bookmarks"].as_array().unwrap();
        assert_eq!(
            profiles[1].as_dictionary().unwrap()["Name"].as_string(),
            Some("profile g2")
        );
    }

    #[test]
    fn set_inserts_missing_key() {
        let mut root = root(Some("g1"), vec![profile("g1", None)], None);
        assert!(set_title_components(&mut root, "g1", 1));
        assert_eq!(title_components_for(&root, "g1"), Some(Some(1)));
    }

    #[test]
    fn set_unknown_guid_returns_false() {
        let mut root = two_profiles();
        assert!(!set_title_components(&mut root, "nope", 1));
        assert_eq!(root, two_profiles());
    }

    #[test]
    fn title_components_for_unknown_guid_is_none() {
        assert_eq!(title_components_for(&two_profiles(), "nope"), None);
    }
}
