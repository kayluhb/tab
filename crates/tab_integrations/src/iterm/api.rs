//! Just enough of iTerm's Python API protocol to set one profile property.
//!
//! The messages mirror iTerm's `api.proto` field numbers. They are written out here rather than
//! generated from that file, which is GPL-licensed.

use std::time::Duration;

use futures::{
    SinkExt as _,
    StreamExt as _,
};
use prost::Message as _;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::{
    Error,
    Result,
};

const TIMEOUT: Duration = Duration::from_secs(5);
const APP_NAME: &str = "Tab";
const REQUEST_ID: i64 = 1;
const STATUS_OK: i32 = 0;

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClientOriginatedMessage {
    #[prost(int64, optional, tag = "1")]
    pub id: Option<i64>,
    #[prost(message, optional, tag = "105")]
    pub set_profile_property_request: Option<SetProfilePropertyRequest>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SetProfilePropertyRequest {
    #[prost(message, optional, tag = "2")]
    pub guid_list: Option<GuidList>,
    #[prost(string, optional, tag = "3")]
    pub key: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub json_value: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GuidList {
    #[prost(string, repeated, tag = "1")]
    pub guids: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ServerOriginatedMessage {
    #[prost(int64, optional, tag = "1")]
    pub id: Option<i64>,
    #[prost(string, optional, tag = "2")]
    pub error: Option<String>,
    #[prost(message, optional, tag = "105")]
    pub set_profile_property_response: Option<SetProfilePropertyResponse>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SetProfilePropertyResponse {
    /// `0` OK, `1` session not found, `2` request malformed, `3` bad GUID.
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
}

/// Sets `key` to `json_value` on the profile `guid` in the running iTerm.
pub async fn set_profile_property(guid: &str, key: &str, json_value: &str) -> Result<()> {
    tokio::time::timeout(TIMEOUT, send(request(REQUEST_ID, guid, key, json_value)))
        .await
        .map_err(|_elapsed| custom("timed out talking to iTerm"))?
}

fn request(id: i64, guid: &str, key: &str, json_value: &str) -> ClientOriginatedMessage {
    ClientOriginatedMessage {
        id: Some(id),
        set_profile_property_request: Some(SetProfilePropertyRequest {
            guid_list: Some(GuidList {
                guids: vec![guid.to_owned()],
            }),
            key: Some(key.to_owned()),
            json_value: Some(json_value.to_owned()),
        }),
    }
}

fn check_response(id: i64, message: &ServerOriginatedMessage) -> Result<()> {
    if message.id != Some(id) {
        return Err(custom("iTerm answered a different request"));
    }
    if let Some(error) = &message.error {
        return Err(Error::Custom(format!("iTerm: {error}").into()));
    }
    match message.set_profile_property_response.as_ref().and_then(|r| r.status) {
        Some(STATUS_OK) => Ok(()),
        status => Err(Error::Custom(
            format!("iTerm refused the change: status {status:?}").into(),
        )),
    }
}

async fn send(message: ClientOriginatedMessage) -> Result<()> {
    let (cookie, key) = cookie_and_key().await?;
    let socket = dirs::home_dir()
        .ok_or_else(|| custom("no home directory"))?
        .join("Library/Application Support/iTerm2/private/socket");
    let stream = UnixStream::connect(socket).await?;

    let mut request = "ws://localhost/".into_client_request().map_err(ws_error)?;
    let headers = request.headers_mut();
    for (name, value) in [
        ("origin", "ws://localhost/"),
        ("sec-websocket-protocol", "api.iterm2.com"),
        ("x-iterm2-advisory-name", APP_NAME),
        ("x-iterm2-disable-auth-ui", "true"),
        ("x-iterm2-library-version", "tab 1"),
        ("x-iterm2-cookie", &cookie),
        ("x-iterm2-key", &key),
    ] {
        headers.insert(
            name,
            HeaderValue::from_str(value).map_err(|err| custom_string(err.to_string()))?,
        );
    }
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(ws_error)?;

    let id = message.id.unwrap_or_default();
    socket
        .send(Message::Binary(message.encode_to_vec().into()))
        .await
        .map_err(ws_error)?;
    while let Some(frame) = socket.next().await {
        if let Message::Binary(bytes) = frame.map_err(ws_error)? {
            let reply =
                ServerOriginatedMessage::decode(bytes.as_ref()).map_err(|err| custom_string(err.to_string()))?;
            if reply.id == Some(id) {
                socket.close(None).await.ok();
                return check_response(id, &reply);
            }
        }
    }
    Err(custom("iTerm closed the connection without answering"))
}

/// Asks iTerm for a one-time cookie. The first time, iTerm asks the user to allow Tab.
async fn cookie_and_key() -> Result<(String, String)> {
    let script = format!(r#"tell application "iTerm2" to request cookie and key for app named "{APP_NAME}""#);
    let output = Command::new("osascript").args(["-e", &script]).output().await?;
    if !output.status.success() {
        return Err(custom_string(format!(
            "osascript: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match stdout.trim().split_once(' ') {
        Some((cookie, key)) => Ok((cookie.to_owned(), key.to_owned())),
        None => Err(custom("unexpected cookie reply from iTerm")),
    }
}

fn custom(message: &'static str) -> Error {
    Error::Custom(message.into())
}

fn custom_string(message: String) -> Error {
    Error::Custom(message.into())
}

fn ws_error(err: tokio_tungstenite::tungstenite::Error) -> Error {
    custom_string(format!("websocket: {err}"))
}

#[cfg(test)]
mod tests {
    use prost::Message as _;

    use super::*;

    // Byte strings produced with iTerm's own Python bindings (`iterm2.api_pb2`).
    const REQUEST: &str = "0807ca063d12260a2442363945363243342d414538312d343041382d413532382d4531304634373533324144331a105469746c6520436f6d706f6e656e7473220131";
    const OK: &str = "0807ca06020800";
    const BAD_GUID: &str = "0807ca06020803";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn decode(hex: &str) -> ServerOriginatedMessage {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        ServerOriginatedMessage::decode(bytes.as_slice()).unwrap()
    }

    #[test]
    fn encodes_set_profile_property_like_iterm() {
        let message = request(7, "B69E62C4-AE81-40A8-A528-E10F47532AD3", "Title Components", "1");
        assert_eq!(hex(&message.encode_to_vec()), REQUEST);
    }

    #[test]
    fn decodes_ok_response() {
        let message = decode(OK);
        assert_eq!(message.id, Some(7));
        assert!(check_response(7, &message).is_ok());
    }

    #[test]
    fn decodes_bad_guid_response() {
        assert!(check_response(7, &decode(BAD_GUID)).is_err());
    }

    #[test]
    fn rejects_error_response() {
        let message = ServerOriginatedMessage {
            id: Some(7),
            error: Some("nope".into()),
            set_profile_property_response: None,
        };
        assert!(check_response(7, &message).is_err());
    }
}
