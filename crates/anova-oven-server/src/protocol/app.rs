//! App-side protocol: the frames Anova's cloud sends a client of
//! `devices.anovaculinary.io`.
//!
//! Every frame is `{command, payload, …}`. Oven state arrives as
//! `EVENT_APO_STATE` with the shared [`OvenState`] under `payload.state`.

use serde::Deserialize;

use super::OvenState;

/// Parse a raw WebSocket message into a protocol event.
pub fn parse_message(data: &[u8]) -> Result<Event, serde_json::Error> {
    let envelope: Envelope = serde_json::from_slice(data)?;
    match envelope.command.as_str() {
        "EVENT_APO_STATE" => {
            let msg: ApoStateMessage = serde_json::from_slice(data)?;
            Ok(Event::ApoState(msg.payload))
        }
        "EVENT_APO_WIFI_LIST" => {
            let v: serde_json::Value = serde_json::from_slice(data)?;
            let cooker_id = v["payload"][0]["cookerId"].as_str().map(String::from);
            Ok(Event::ApoWifiList { cooker_id })
        }
        "EVENT_USER_STATE" => Ok(Event::UserState),
        "RESPONSE" => {
            let msg: ResponseMessage = serde_json::from_slice(data)?;
            Ok(Event::Response {
                request_id: msg.request_id,
                status: msg.payload.status,
            })
        }
        _ => Ok(Event::Unknown),
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    ApoState(ApoStatePayload),
    ApoWifiList { cooker_id: Option<String> },
    UserState,
    Response { request_id: String, status: String },
    Unknown,
}

// --- Wire format types ---

#[derive(Deserialize)]
struct Envelope {
    command: String,
}

#[derive(Deserialize)]
struct ApoStateMessage {
    payload: ApoStatePayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseMessage {
    request_id: String,
    payload: ResponsePayload,
}

#[derive(Deserialize)]
struct ResponsePayload {
    status: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApoStatePayload {
    #[allow(dead_code)]
    pub cooker_id: String,
    pub state: OvenState,
}
