use biome_deserialize_macros::{Deserializable, Merge};
use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Debug, Deserialize, Deserializable, Merge, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct UseThisForClassMethodsOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub except_methods: Option<Box<[Box<str>]>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_override_methods: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_classes_with_implements: Option<IgnoreClassesWithImplements>,
}

#[derive(Clone, Copy, Debug, Deserialize, Deserializable, Eq, Merge, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum IgnoreClassesWithImplements {
    #[serde(rename = "all")]
    All,

    #[serde(rename = "public-fields")]
    PublicFields,
}
