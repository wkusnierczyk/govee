use serde::{Deserialize, Serialize};

/// Top-level capability entry returned by the Govee v2 API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    #[serde(rename = "type")]
    pub type_: String,
    pub instance: String,
    pub parameters: CapabilityParameters,
}

/// The typed parameters for a capability, tagged by `dataType`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "dataType")]
pub enum CapabilityParameters {
    #[serde(rename = "ENUM")]
    Enum { options: Vec<EnumOption> },
    #[serde(rename = "INTEGER")]
    Integer(IntRange),
    #[serde(rename = "STRUCT")]
    Struct { fields: Vec<StructField> },
    /// Forward-compatibility catch-all for unknown `dataType` values.
    #[serde(other)]
    Unknown,
}

/// A single option in an ENUM capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumOption {
    pub name: String,
    pub value: serde_json::Value,
}

/// The range descriptor for an INTEGER capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntRange {
    pub min: i64,
    pub max: i64,
    pub precision: i64,
    pub unit: Option<String>,
}

/// A single field in a STRUCT capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructField {
    pub field_name: String,
    pub data_type: String,
}

/// The current state of a single capability instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityState {
    #[serde(rename = "type")]
    pub type_: String,
    pub instance: String,
    pub state: StateValue,
}

/// The value wrapper inside a `CapabilityState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateValue {
    pub value: serde_json::Value,
}

/// Control value for issuing commands to a device.
///
/// This enum is **not** serde-deserializable — it is constructed in code.
#[derive(Debug, Clone)]
pub enum CapabilityValue {
    OnOff(u8),
    Rgb(u32),
    ColorTempK(u32),
    Brightness(u8),
    WorkMode {
        work_mode: u32,
        mode_value: Option<u32>,
    },
    DynamicScene(DynamicSceneValue),
    DiyScene(u32),
    SegmentColor {
        segments: Vec<u8>,
        rgb: u32,
    },
    SegmentBrightness {
        segments: Vec<u8>,
        brightness: u8,
    },
    Raw(serde_json::Value),
}

/// A dynamic scene identifier, which may be either a preset (with a `paramId`) or a DIY index.
///
/// Uses `#[serde(untagged)]` — `Preset` is tried first (order matters).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DynamicSceneValue {
    /// A preset scene identified by `paramId` and `id`.
    Preset {
        #[serde(rename = "paramId")]
        param_id: u32,
        id: u32,
    },
    /// A DIY scene identified by a plain integer index.
    Diy(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_parameters_enum_variant() {
        let json =
            r#"{"dataType":"ENUM","options":[{"name":"on","value":1},{"name":"off","value":0}]}"#;
        let p: CapabilityParameters = serde_json::from_str(json).unwrap();
        match p {
            CapabilityParameters::Enum { options } => {
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].name, "on");
                assert_eq!(options[0].value, serde_json::json!(1));
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn capability_parameters_integer_variant() {
        let json = r#"{"dataType":"INTEGER","min":0,"max":100,"precision":1,"unit":"percent"}"#;
        let p: CapabilityParameters = serde_json::from_str(json).unwrap();
        match p {
            CapabilityParameters::Integer(range) => {
                assert_eq!(range.min, 0);
                assert_eq!(range.max, 100);
                assert_eq!(range.precision, 1);
                assert_eq!(range.unit.as_deref(), Some("percent"));
            }
            other => panic!("expected Integer, got {other:?}"),
        }
    }

    #[test]
    fn capability_parameters_struct_variant() {
        let json = r#"{"dataType":"STRUCT","fields":[{"fieldName":"colorTemInKelvin","dataType":"INTEGER"}]}"#;
        let p: CapabilityParameters = serde_json::from_str(json).unwrap();
        match p {
            CapabilityParameters::Struct { fields } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].field_name, "colorTemInKelvin");
                assert_eq!(fields[0].data_type, "INTEGER");
            }
            other => panic!("expected Struct, got {other:?}"),
        }
    }

    #[test]
    fn capability_parameters_unknown_variant() {
        let json = r#"{"dataType":"FUTURE_TYPE","someField":42}"#;
        let p: CapabilityParameters = serde_json::from_str(json).unwrap();
        assert!(
            matches!(p, CapabilityParameters::Unknown),
            "expected Unknown, got {p:?}"
        );
    }

    #[test]
    fn dynamic_scene_value_preset() {
        let json = r#"{"paramId":1,"id":2}"#;
        let v: DynamicSceneValue = serde_json::from_str(json).unwrap();
        match v {
            DynamicSceneValue::Preset { param_id, id } => {
                assert_eq!(param_id, 1);
                assert_eq!(id, 2);
            }
            other => panic!("expected Preset, got {other:?}"),
        }
    }

    #[test]
    fn dynamic_scene_value_diy() {
        let json = "42";
        let v: DynamicSceneValue = serde_json::from_str(json).unwrap();
        match v {
            DynamicSceneValue::Diy(n) => assert_eq!(n, 42),
            other => panic!("expected Diy, got {other:?}"),
        }
    }

    #[test]
    fn capability_state_round_trip() {
        let original = CapabilityState {
            type_: "devices.capabilities.on_off".to_string(),
            instance: "powerSwitch".to_string(),
            state: StateValue {
                value: serde_json::json!(1),
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: CapabilityState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.type_, original.type_);
        assert_eq!(deserialized.instance, original.instance);
        assert_eq!(deserialized.state.value, original.state.value);
    }
}
