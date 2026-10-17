//! Schema generation and the field contract derived from it.
//!
//! Every request type derives its JSON Schema with `schemars`. `input_schema`
//! renders that schema in the flat draft-07 form hosted clients accept, and
//! `Contract` reads the same schema back to enforce the fields a request may
//! carry. The type is therefore the single definition of the published schema,
//! the runtime field checks and the typed value handlers receive.

use crate::error::{AppError, AppResult, ErrorCode};
use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// The draft-07 input schema clients see for `T`.
///
/// `schemars` describes optional fields as nullable, adds integer `format`
/// hints and writes single-value tags as `const`. The published schemas never
/// used those keywords, so they are rewritten here: `null` is dropped from
/// `type` and `enum`, `format` is removed and `const` becomes a one-value
/// `enum`. A request still accepts `null` for an optional field, which counts
/// as omitted.
pub fn input_schema<T: JsonSchema>() -> Map<String, Value> {
    let generator = SchemaSettings::draft07()
        .with(|settings| settings.inline_subschemas = true)
        .into_generator();
    let mut root = match generator.into_root_schema_for::<T>().to_value() {
        Value::Object(root) => root,
        _ => Map::new(),
    };
    root.remove("title");
    normalize(&mut root);
    root
}

fn normalize(schema: &mut Map<String, Value>) {
    schema.remove("format");
    if schema.get("default").is_some_and(Value::is_null) {
        schema.remove("default");
    }
    if let Some(Value::Array(types)) = schema.get("type") {
        let mut types = types
            .iter()
            .filter(|kind| kind.as_str() != Some("null"))
            .cloned()
            .collect::<Vec<_>>();
        let kind = if types.len() == 1 {
            types.remove(0)
        } else {
            Value::Array(types)
        };
        schema.insert("type".to_owned(), kind);
    }
    if let Some(Value::Array(values)) = schema.get_mut("enum") {
        values.retain(|value| !value.is_null());
    }
    if let Some(value) = schema.remove("const") {
        schema.insert("enum".to_owned(), json!([value]));
    }
    if let Some(Value::Object(items)) = schema.get_mut("items") {
        normalize(items);
    }
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        properties.values_mut().for_each(normalize_value);
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = schema.get_mut(keyword) {
            branches.iter_mut().for_each(normalize_value);
        }
    }
}

fn normalize_value(schema: &mut Value) {
    if let Value::Object(schema) = schema {
        normalize(schema);
    }
}

/// How a request boundary reports a rejected field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// A tool argument: `UNKNOWN_TOOL_FIELD` and `INVALID_ARGUMENT`.
    Argument,
    /// A `bash*` tool argument: `UNKNOWN_TOOL_FIELD` and `INVALID_BASH_REQUEST`.
    Bash,
    /// A narrow edit tool argument: `UNKNOWN_TOOL_FIELD`, `MISSING_CHANGE_FIELD`
    /// and `INVALID_CHANGE_FIELD`.
    Change,
    /// One item of a `changes` array: the change error codes throughout.
    ChangeItem,
}

impl Rejection {
    fn unknown(self, contract: &Contract, unknown: Vec<String>) -> AppError {
        match self {
            Self::ChangeItem => AppError::with_details(
                ErrorCode::UnknownChangeField,
                "change received unsupported fields",
                json!({
                    "contract": contract.name,
                    "unknown": unknown,
                    "allowed": contract.allowed
                }),
            ),
            Self::Argument | Self::Bash | Self::Change => AppError::with_details(
                ErrorCode::UnknownToolField,
                format!("tool '{}' received unsupported fields", contract.name),
                json!({"unknown": unknown, "allowed": contract.allowed}),
            ),
        }
    }

    fn missing(self, contract: &Contract, missing: Vec<&str>) -> AppError {
        let message = format!("{} is missing required fields", contract.name);
        let details = json!({
            "contract": contract.name,
            "missing": missing,
            "required": contract.required
        });
        match self {
            Self::Argument => AppError::with_details(ErrorCode::InvalidArgument, message, details),
            Self::Bash => AppError::with_details(ErrorCode::InvalidBashRequest, message, details),
            Self::Change | Self::ChangeItem => {
                AppError::with_details(ErrorCode::MissingChangeField, message, details)
            }
        }
    }

    /// A field that is present but has the wrong type or value.
    pub fn invalid(self, message: impl Into<String>, details: Value) -> AppError {
        match self {
            Self::Argument => AppError::with_details(ErrorCode::InvalidArgument, message, details),
            Self::Bash => AppError::with_details(ErrorCode::InvalidBashRequest, message, details),
            Self::Change | Self::ChangeItem => {
                AppError::with_details(ErrorCode::InvalidChangeField, message, details)
            }
        }
    }
}

/// Bounds the schema declares for one integer field.
#[derive(Debug, Clone)]
struct IntegerBounds {
    field: String,
    minimum: Option<i128>,
    maximum: Option<i128>,
}

/// The fields one request object may carry, read from its schema.
#[derive(Debug, Clone)]
pub struct Contract {
    name: String,
    allowed: BTreeSet<String>,
    required: Vec<String>,
    integers: Vec<IntegerBounds>,
    min_lengths: Vec<(String, u64)>,
}

impl Contract {
    /// The contract of the object schema `schema`, reported as `name`.
    pub fn from_schema(name: &str, schema: &Map<String, Value>) -> Self {
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let bound = |property: &Value, keyword: &str| {
            let value = property.get(keyword)?;
            value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
        };
        let integers = properties
            .iter()
            .filter(|(_, property)| property["type"] == "integer")
            .map(|(field, property)| IntegerBounds {
                field: field.clone(),
                minimum: bound(property, "minimum"),
                maximum: bound(property, "maximum"),
            })
            .collect();
        let min_lengths = properties
            .iter()
            .filter(|(_, property)| property["type"] == "string")
            .filter_map(|(field, property)| {
                property
                    .get("minLength")
                    .and_then(Value::as_u64)
                    .map(|length| (field.clone(), length))
            })
            .collect();
        Self {
            name: name.to_owned(),
            allowed: properties.keys().cloned().collect(),
            required,
            integers,
            min_lengths,
        }
    }

    /// The contract of `T`'s generated schema.
    pub fn of<T: JsonSchema>(name: &str) -> Self {
        Self::from_schema(name, &input_schema::<T>())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Validate `value` against this contract and deserialize it into `T`.
    ///
    /// JSON `null` counts as an omitted field. Unknown fields are reported
    /// before missing ones; integer bounds and minimum string lengths come from
    /// the schema; every remaining type error comes from deserialization.
    pub fn parse<T: DeserializeOwned>(&self, value: Value, rejection: Rejection) -> AppResult<T> {
        let Value::Object(mut object) = value else {
            return Err(rejection.invalid(
                format!("{} input must be an object", self.name),
                json!({"contract": self.name}),
            ));
        };
        self.check(&mut object, rejection)?;
        serde_path_to_error::deserialize(Value::Object(object)).map_err(|error| {
            let field = error.path().to_string();
            rejection.invalid(
                format!(
                    "{} field '{field}' is invalid: {}",
                    self.name,
                    error.inner()
                ),
                json!({
                    "contract": self.name,
                    "field": field,
                    "error": error.inner().to_string()
                }),
            )
        })
    }

    fn check(&self, object: &mut Map<String, Value>, rejection: Rejection) -> AppResult<()> {
        object.retain(|_, value| !value.is_null());
        let unknown = object
            .keys()
            .filter(|field| !self.allowed.contains(*field))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(rejection.unknown(self, unknown));
        }
        let missing = self
            .required
            .iter()
            .filter(|field| !object.contains_key(*field))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(rejection.missing(self, missing));
        }
        for bounds in &self.integers {
            let Some(value) = object.get(&bounds.field) else {
                continue;
            };
            let Some(number) = value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
            else {
                continue;
            };
            let below = bounds.minimum.is_some_and(|minimum| number < minimum);
            let above = bounds.maximum.is_some_and(|maximum| number > maximum);
            if below || above {
                let range = match (bounds.minimum, bounds.maximum) {
                    (Some(minimum), Some(maximum)) => format!("from {minimum} to {maximum}"),
                    (Some(minimum), None) => format!("of at least {minimum}"),
                    (None, Some(maximum)) => format!("of at most {maximum}"),
                    (None, None) => String::new(),
                };
                return Err(rejection.invalid(
                    format!("{} must be an integer {range}", bounds.field),
                    json!({"contract": self.name, "field": bounds.field, "value": value}),
                ));
            }
        }
        for (field, minimum) in &self.min_lengths {
            let too_short = object
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|text| (text.chars().count() as u64) < *minimum);
            if too_short {
                return Err(rejection.invalid(
                    format!(
                        "{} field '{field}' must be at least {minimum} characters",
                        self.name
                    ),
                    json!({"contract": self.name, "field": field}),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        #[schemars(length(min = 1))]
        name: String,
        #[schemars(range(min = 1, max = 10))]
        count: Option<u32>,
    }

    fn parse(value: Value) -> AppResult<Sample> {
        Contract::of::<Sample>("sample").parse(value, Rejection::Argument)
    }

    #[test]
    fn schema_is_flat_draft_07_without_nullable_types_or_formats() {
        assert_eq!(
            Value::Object(input_schema::<Sample>()),
            json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "name": {"type": "string", "minLength": 1},
                    "count": {"type": "integer", "minimum": 1, "maximum": 10}
                },
                "required": ["name"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn parse_accepts_null_as_omitted_and_rejects_out_of_contract_values() {
        assert_eq!(
            parse(json!({"name": "a", "count": null})).unwrap(),
            Sample {
                name: "a".to_owned(),
                count: None
            }
        );

        let codes = [
            json!({"name": "a", "extra": 1}),
            json!({"count": 2}),
            json!({"name": "a", "count": 11}),
            json!({"name": "a", "count": "2"}),
            json!({"name": ""}),
            json!(["not", "an", "object"]),
        ]
        .map(|value| parse(value).unwrap_err().code().as_str());

        assert_eq!(
            codes,
            [
                "UNKNOWN_TOOL_FIELD",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT"
            ]
        );
    }

    #[test]
    fn change_items_report_change_codes() {
        let contract = Contract::of::<Sample>("sample");
        let unknown = contract
            .parse::<Sample>(json!({"name": "a", "extra": 1}), Rejection::ChangeItem)
            .unwrap_err();
        let missing = contract
            .parse::<Sample>(json!({}), Rejection::ChangeItem)
            .unwrap_err();

        assert_eq!(unknown.code(), ErrorCode::UnknownChangeField);
        assert_eq!(unknown.details().unwrap()["unknown"], json!(["extra"]));
        assert_eq!(missing.code(), ErrorCode::MissingChangeField);
        assert_eq!(missing.details().unwrap()["missing"], json!(["name"]));
    }
}
