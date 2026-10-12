//! Wire names are camelCase on output, and the snake_case field names that
//! version 1 hosts and clients used are still accepted on input.

use blitz_control_protocol::{
    AgentAction, AgentControlRequest, DEBUG_PROTOCOL_VERSION, InputCommand, MAX_DEBUG_FRAME_BYTES,
    Modifiers, WheelPhase,
};
use serde_json::{Value, json};

fn round_trip<T>(value: &T) -> Value
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = serde_json::to_value(value).expect("encode");
    let decoded: T = serde_json::from_value(encoded.clone()).expect("decode");
    assert_eq!(&decoded, value);
    encoded
}

#[test]
fn inspect_emits_camel_case_and_accepts_snake_case() {
    let request = AgentControlRequest::Inspect {
        root: Some(3),
        max_depth: 4,
    };
    let encoded = round_trip(&request);
    assert_eq!(
        encoded,
        json!({"command":"inspect","params":{"root":3,"maxDepth":4}})
    );
    let legacy: AgentControlRequest =
        serde_json::from_value(json!({"command":"inspect","params":{"root":3,"max_depth":4}}))
            .expect("legacy max_depth must decode");
    assert_eq!(legacy, request);
}

#[test]
fn every_node_action_emits_node_id_and_accepts_node_id_alias() {
    let actions = [
        AgentAction::Focus { node_id: 9 },
        AgentAction::Click { node_id: 9 },
        AgentAction::DoubleClick { node_id: 9 },
        AgentAction::Hover { node_id: 9 },
        AgentAction::SetValue {
            node_id: 9,
            value: "x".to_owned(),
        },
        AgentAction::ScrollIntoView { node_id: 9 },
    ];
    for action in actions {
        let mut encoded = round_trip(&action);
        let params = encoded["params"].as_object_mut().expect("params");
        assert_eq!(params.get("nodeId"), Some(&json!(9)), "{action:?}");
        assert!(!params.contains_key("node_id"), "{action:?}");
        let id = params.remove("nodeId").expect("nodeId");
        params.insert("node_id".to_owned(), id);
        let legacy: AgentAction = serde_json::from_value(encoded).expect("legacy node_id");
        assert_eq!(legacy, action);
    }
}

#[test]
fn scroll_and_wheel_deltas_are_camel_case_with_snake_case_aliases() {
    let scroll = AgentAction::ScrollBy {
        node_id: 2,
        delta_x: 1.0,
        delta_y: -2.0,
    };
    assert_eq!(
        round_trip(&scroll),
        json!({"action":"scrollBy","params":{"nodeId":2,"deltaX":1.0,"deltaY":-2.0}})
    );
    let legacy: AgentAction = serde_json::from_value(
        json!({"action":"scrollBy","params":{"node_id":2,"delta_x":1.0,"delta_y":-2.0}}),
    )
    .expect("legacy scroll");
    assert_eq!(legacy, scroll);

    let wheel = InputCommand::Wheel {
        delta_x: 0.0,
        delta_y: 5.0,
        phase: WheelPhase::Moved,
        modifiers: Modifiers::default(),
    };
    let encoded = round_trip(&wheel);
    assert_eq!(encoded["deltaY"], json!(5.0));
    let mut legacy = encoded.clone();
    let object = legacy.as_object_mut().expect("object");
    let y = object.remove("deltaY").expect("deltaY");
    let x = object.remove("deltaX").expect("deltaX");
    object.insert("delta_y".to_owned(), y);
    object.insert("delta_x".to_owned(), x);
    let legacy: InputCommand = serde_json::from_value(legacy).expect("legacy wheel");
    assert_eq!(legacy, wheel);
}

#[test]
fn frame_limit_and_version_are_the_published_values() {
    assert_eq!(MAX_DEBUG_FRAME_BYTES, 16 * 1024 * 1024);
    assert_eq!(DEBUG_PROTOCOL_VERSION, 2);
}
