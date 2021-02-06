// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::matrix::protocol::{Event, JoinedRoomSyncResponse};
use ruma_client::api::r0::sync::sync_events;

use std::collections::{BTreeMap, BTreeSet};

use ruma_client::identifiers::RoomId;
use serde_json::Value;

pub struct Room {
    room_id: RoomId,
    state: BTreeMap<String, BTreeMap<String, Event>>,
    members: BTreeMap<String, Member>,
}

impl Room {
    pub fn from_sync(room_id: RoomId, resp: &sync_events::JoinedRoom) -> Room {
        let mut state_map = BTreeMap::new();

        for ev in &resp.state.events {
            let state_key = ev
                .state_key
                .as_ref()
                .expect("state should have state_key")
                .to_string();
            state_map
                .entry(ev.etype.clone())
                .or_insert_with(BTreeMap::new)
                .insert(state_key, ev.clone());
        }

        for ev in &resp.timeline.events {
            if let Some(ref state_key) = ev.state_key {
                state_map
                    .entry(ev.etype.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(state_key.clone(), ev.clone());
            }
        }

        let mut room = Room {
            room_id,
            state: state_map,
            members: BTreeMap::new(),
        };

        room.recalculate_members();

        room
    }

    pub fn update_from_sync(&mut self, resp: &sync_events::JoinedRoom) {
        let mut contained_member_event = false;

        for ev in &resp.state.events {
            contained_member_event |= ev.etype == "m.room_member";

            let state_key = ev
                .state_key
                .as_ref()
                .expect("state should have state_key")
                .to_string();
            self.state
                .entry(ev.etype.clone())
                .or_insert_with(BTreeMap::new)
                .insert(state_key, ev.clone());
        }

        for ev in &resp.timeline.events {
            if let Some(ref state_key) = ev.state_key {
                contained_member_event |= ev.etype == "m.room_member";

                self.state
                    .entry(ev.etype.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(state_key.clone(), ev.clone());
            }
        }

        if contained_member_event {
            self.recalculate_members();
        }
    }

    fn recalculate_members(&mut self) {
        let mut mods = BTreeSet::new();
        {
            let mod_map = self
                .get_state("m.room.power_levels", "")
                .and_then(|content| content.get("users"))
                .and_then(Value::as_object);

            if let Some(users) = mod_map {
                for (user_id, level_val) in users {
                    if let Some(level) = level_val.as_u64() {
                        if level >= 50 {
                            mods.insert(user_id.clone());
                        }
                    }
                }
            }
        }
        {
            let Room {
                ref state,
                ref mut members,
                ..
            } = *self;

            if let Some(member_state) = state.get("m.room.member") {
                for (user_id, ev) in member_state {
                    let membership = ev
                        .content
                        .get("membership")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let display_name = ev
                        .content
                        .get("displayname")
                        .and_then(Value::as_str)
                        .unwrap_or("");

                    if membership == "join" {
                        members.insert(
                            user_id.clone(),
                            Member {
                                user_id: user_id.clone(),
                                display_name: display_name.into(),
                                moderator: mods.contains(user_id),
                            },
                        );
                    }
                }
            }
        }
    }

    pub fn get_name(&self) -> Option<&str> {
        self.get_state_content_key("m.room.name", "", "name")
    }

    pub fn get_topic(&self) -> Option<&str> {
        self.get_state_content_key("m.room.topic", "", "topic")
    }

    pub fn get_state_content_key(
        &self,
        etype: &str,
        state_key: &str,
        content_key: &str,
    ) -> Option<&str> {
        self.get_state(etype, state_key)
            .and_then(Value::as_object)
            .and_then(|content| content.get(content_key))
            .and_then(Value::as_str)
    }

    pub fn get_state(&self, etype: &str, state_key: &str) -> Option<&Value> {
        self.state
            .get(etype)
            .and_then(|topic_map| topic_map.get(state_key))
            .map(|topic_ev| &topic_ev.content)
    }

    pub fn get_members(&self) -> &BTreeMap<String, Member> {
        &self.members
    }

    pub fn get_room_id(&self) -> &RoomId {
        &self.room_id
    }
}

pub struct Member {
    pub user_id: String,
    pub display_name: String,
    pub moderator: bool,
}
