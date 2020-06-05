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

use std::collections::BTreeMap;

use serde_json;
use serde::{Serialize, Deserialize};


#[derive(Debug, Clone, Deserialize)]
pub struct SyncResponse {
    pub next_batch: String,
    pub rooms: RoomsSyncResponse,
}


#[derive(Debug, Clone, Deserialize)]
pub struct RoomsSyncResponse {
    pub join: BTreeMap<String, JoinedRoomSyncResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JoinedRoomSyncResponse {
    pub timeline: RoomTimelineSyncResponse,
    pub state: RoomStateSyncResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomTimelineSyncResponse {
    pub limited: bool,
    pub prev_batch: String,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomStateSyncResponse {
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub sender: String,
    pub event_id: String,
    #[serde(rename = "type")]
    pub etype: String,
    pub state_key: Option<String>,
    pub content: serde_json::Value,
    pub unsigned: EventUnsigned,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventUnsigned {
    pub transaction_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginPasswordInput {
    pub user: String,
    pub password: String,
    #[serde(rename = "type")]
    pub login_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub device_id: String,
    pub home_server: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomJoinInput {
    // third_party_signed at some point
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomJoinResponse {
    pub room_id: String,
}


#[derive(Debug, Clone, Serialize)]
pub struct RoomSendInput {
    pub msgtype: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomSendResponse {
    pub event_id: String,
}


#[cfg(test)]
mod tests {
    use serde_json;
    use super::*;

    #[test]
    fn sync_response() {
        let sync_response_str = r#"
        {
          "next_batch": "s2240646_7037295_67564_1482345_530_40_551",
          "rooms": {
            "leave": {},
            "join": {
              "!SDFsdfqsf24SB:matrix.org": {
                "unread_notifications": {
                  "highlight_count": 0,
                  "notification_count": 0
                },
                "timeline": {
                  "limited": false,
                  "prev_batch": "s2240646_7037295_67564_1482345_530_40_551",
                  "events": [
                    {
                      "origin_server_ts": 1475512030637,
                      "sender": "@wibble:matrix.org",
                      "event_id": "$147sdfsdfsdfKBLVL:matrix.org",
                      "unsigned": {
                        "age": 1008
                      },
                      "content": {
                        "body": "test test test test",
                        "msgtype": "m.text"
                      },
                      "type": "m.room.message"
                    }
                  ]
                },
                "state": {
                  "events": []
                },
                "ephemeral": {
                  "events": []
                },
                "account_data": {
                  "events": []
                }
              }
            },
            "invite": {}
          },
          "account_data": {
            "events": []
          },
          "to_device": {
            "events": []
          },
          "presence": {}
        }
        "#;

        let _parsed: SyncResponse = serde_json::from_str(sync_response_str).unwrap();
    }
}
