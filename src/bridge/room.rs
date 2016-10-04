use matrix::models::{Event, JoinedRoomSyncResponse};
use irc::IrcUserConnection;

use std::collections::BTreeMap;

use serde_json::Value;


pub struct Room {
    pub irc_name: String,
    pub room_id: String,
    pub state: BTreeMap<String, BTreeMap<String, Event>>,
}

impl Room {
    pub fn from_sync(room_id: String, resp: &JoinedRoomSyncResponse) -> Room {
        let mut state_map = BTreeMap::new();

        for ev in &resp.state.events {
            // state_map.insert((ev.etype.clone(), ev.state_key.as_ref().unwrap().to_string()), ev.clone());
            state_map.entry(ev.etype.clone()).or_insert_with(BTreeMap::new).insert(ev.state_key.as_ref().unwrap().to_string(), ev.clone());
        }

        for ev in &resp.timeline.events {
            if let Some(ref state_key) = ev.state_key {
                // state_map.insert((ev.etype.clone(), state_key.clone()), ev.clone());
                state_map.entry(ev.etype.clone()).or_insert_with(BTreeMap::new).insert(state_key.clone(), ev.clone());
            }
        }

        Room {
            irc_name: state_to_room_name(&room_id, &state_map),
            room_id: room_id,
            state: state_map,
        }
    }
}


fn state_to_room_name(room_id: &str, state: &BTreeMap<String, BTreeMap<String, Event>>) -> String {
    if let Some(canonical_alias) = state.get("m.room.canonical_alias").and_then(|evs| evs.get("")) {
        if let Some(alias) = canonical_alias.content.find("canonical_alias").and_then(Value::as_str) {
            return alias.into();
        }
    }

    if let Some(canonical_alias) = state.get("m.room.name").and_then(|evs| evs.get("")) {
        if let Some(name) = canonical_alias.content.find("name").and_then(Value::as_str) {
            let stripped_name: String = name.chars().filter(|c| match *c {
                '\x00' ... '\x20' | '"' | '+' | '#' | '\x7F' => false,
                _ => true,
            }).collect();

            if !stripped_name.is_empty() {
                return format!("#{}", stripped_name);
            }
        }
    }

    format!("#{}", room_id)
}
