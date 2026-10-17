//! Edit tool requests: the six narrow single-change tools and the
//! `code_preview` / `code_transaction` change lists, all compiled into one
//! `EditRequest` for the edit engine.

use super::contract::{input_schema, Contract, Rejection};
use crate::contracts::ErrorCode;
use crate::model::AppResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::OnceLock;

const HANDLE_DESCRIPTION: &str = "Fresh range handle returned by a code_retrieve read. It binds the workspace, path, line range, and content hash. A handle-based change must be the only change for its file in one transaction; combining it with another change for that file is rejected as ambiguous.";
const OPTIONAL_HANDLE_DESCRIPTION: &str = "Optional fresh range handle returned by a code_retrieve read. It scopes the exact-text match to the fetched range and binds the workspace, path, and content hash. A handle-based change must be the only change for its file in one transaction; combining it with another change for that file is rejected as ambiguous.";
const REPLACEMENT_TEXT_DESCRIPTION: &str = "Replacement text. When replacing text that ends with a terminal newline, omitting that newline here preserves it from the selected text or range.";
const VALIDATION_DESCRIPTION: &str = "Post-apply validation commands. The edit remains applied if a command fails; long-running validation may continue in the background.";
const CHANGE_DESCRIPTION: &str = "One operation-specific edit change. Select the branch whose kind matches the requested operation.";
const TRANSACTION_DETAIL_DESCRIPTION: &str = "compact omits the unified diff and returns diff_stat only; standard caps the diff to bound payload size; debug returns the full diff.";

// How much of the diff an applied edit returns. (Plain comments: schemars
// would publish doc comments as schema descriptions.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseDetail {
    Compact,
    #[default]
    Standard,
    Debug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InsertPosition {
    Before,
    After,
    InsideStart,
    InsideEnd,
}

// One typed change. The `kind` tag selects the variant and each variant
// rejects fields that belong to another kind.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(extend("type" = "object"), description = CHANGE_DESCRIPTION)]
pub enum Change {
    #[schemars(title = "Create change")]
    Create {
        path: String,
        content: String,
        #[serde(default)]
        overwrite: bool,
        expected_hash: Option<String>,
    },
    #[schemars(title = "Replace change")]
    Replace {
        path: String,
        old_text: String,
        #[schemars(description = REPLACEMENT_TEXT_DESCRIPTION)]
        new_text: String,
        #[serde(default = "one")]
        #[schemars(range(min = 1))]
        expected_replacements: usize,
        expected_hash: Option<String>,
        #[schemars(description = OPTIONAL_HANDLE_DESCRIPTION)]
        handle: Option<String>,
    },
    #[schemars(title = "Replace range change")]
    ReplaceRange {
        path: String,
        #[schemars(description = HANDLE_DESCRIPTION)]
        handle: String,
        #[schemars(description = REPLACEMENT_TEXT_DESCRIPTION)]
        new_text: String,
    },
    #[schemars(title = "Insert change")]
    Insert {
        path: String,
        content: String,
        anchor_symbol: String,
        position: InsertPosition,
        expected_hash: Option<String>,
    },
    #[schemars(title = "Delete change")]
    Delete {
        path: String,
        expected_hash: Option<String>,
    },
    #[schemars(title = "Rename change")]
    Rename {
        path: String,
        to: String,
        expected_hash: Option<String>,
    },
}

fn one() -> usize {
    1
}

fn yes() -> bool {
    true
}

/// The per-kind contracts, read from the `oneOf` branches of `Change`'s schema.
fn change_contracts() -> &'static [Contract] {
    static CONTRACTS: OnceLock<Vec<Contract>> = OnceLock::new();
    CONTRACTS.get_or_init(|| {
        let schema = input_schema::<Change>();
        schema
            .get("oneOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter_map(|branch| {
                let kind = branch
                    .get("properties")?
                    .get("kind")?
                    .get("enum")?
                    .get(0)?
                    .as_str()?;
                Some(Contract::from_schema(kind, branch))
            })
            .collect()
    })
}

impl Change {
    /// Parse one item of a `changes` array.
    pub fn parse(value: Value) -> AppResult<Self> {
        let Some(object) = value.as_object() else {
            return Err(ErrorCode::InvalidChangeField.error(
                "transaction changes must be objects",
                json!({"field": "changes"}),
            ));
        };
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .filter(|kind| !kind.trim().is_empty())
            .ok_or_else(|| {
                ErrorCode::MissingChangeField
                    .error("change requires kind", json!({"field": "kind"}))
            })?;
        let contracts = change_contracts();
        let contract = contracts
            .iter()
            .find(|contract| contract.name() == kind)
            .ok_or_else(|| {
                ErrorCode::UnsupportedChangeKind.error(
                    format!("unsupported change kind '{kind}'"),
                    json!({
                        "kind": kind,
                        "supported": contracts.iter().map(Contract::name).collect::<Vec<_>>()
                    }),
                )
            })?;
        let change: Self = contract.parse(value, Rejection::ChangeItem)?;
        change.validate()?;
        Ok(change)
    }

    /// Check the values a type cannot express: a non-blank path and handle.
    fn validate(&self) -> AppResult<()> {
        let mut required = vec![("path", self.path())];
        if let Self::ReplaceRange { handle, .. } = self {
            required.push(("handle", handle.as_str()));
        }
        match required
            .into_iter()
            .find(|(_, value)| value.trim().is_empty())
        {
            Some((field, _)) => Err(ErrorCode::InvalidChangeField.error(
                format!("change field '{field}' must be a non-empty string"),
                json!({"field": field}),
            )),
            None => Ok(()),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Replace { .. } => "replace",
            Self::ReplaceRange { .. } => "replace_range",
            Self::Insert { .. } => "insert",
            Self::Delete { .. } => "delete",
            Self::Rename { .. } => "rename",
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Create { path, .. }
            | Self::Replace { path, .. }
            | Self::ReplaceRange { path, .. }
            | Self::Insert { path, .. }
            | Self::Delete { path, .. }
            | Self::Rename { path, .. } => path,
        }
    }

    pub fn expected_hash(&self) -> Option<&str> {
        match self {
            Self::Create { expected_hash, .. }
            | Self::Replace { expected_hash, .. }
            | Self::Insert { expected_hash, .. }
            | Self::Delete { expected_hash, .. }
            | Self::Rename { expected_hash, .. } => expected_hash.as_deref(),
            Self::ReplaceRange { .. } => None,
        }
    }

    pub fn handle(&self) -> Option<&str> {
        match self {
            Self::Replace { handle, .. } => handle.as_deref(),
            Self::ReplaceRange { handle, .. } => Some(handle),
            _ => None,
        }
    }
}

/// One validated edit for the edit engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
    pub changes: Vec<Change>,
    /// Snapshot precondition; the application fills in the current snapshot
    /// when the caller sends none.
    pub snapshot_id: Option<String>,
    /// Plan and diff without writing.
    pub preview: bool,
    /// Post-apply validation commands.
    pub validate: Vec<String>,
    pub response_detail: ResponseDetail,
}

impl EditRequest {
    fn single(
        change: Change,
        validate: Option<Vec<String>>,
        response_detail: ResponseDetail,
    ) -> AppResult<Self> {
        change.validate()?;
        Ok(Self {
            changes: vec![change],
            snapshot_id: None,
            preview: false,
            validate: validate.unwrap_or_default(),
            response_detail,
        })
    }

    fn batch(
        changes: Vec<Value>,
        snapshot_id: Option<String>,
        preview: bool,
        validate: Option<Vec<String>>,
        response_detail: ResponseDetail,
    ) -> AppResult<Self> {
        Ok(Self {
            changes: changes
                .into_iter()
                .map(Change::parse)
                .collect::<AppResult<_>>()?,
            snapshot_id,
            preview,
            validate: validate.unwrap_or_default(),
            response_detail,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeWriteRequest {
    pub path: String,
    pub content: String,
    #[serde(default = "yes")]
    pub overwrite: bool,
    pub expected_hash: Option<String>,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub response_detail: ResponseDetail,
}

impl CodeWriteRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        let change = Change::Create {
            path: self.path,
            content: self.content,
            overwrite: self.overwrite,
            expected_hash: self.expected_hash,
        };
        EditRequest::single(change, self.validate, self.response_detail)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeReplaceRequest {
    pub path: String,
    pub old_text: String,
    #[schemars(description = REPLACEMENT_TEXT_DESCRIPTION)]
    pub new_text: String,
    #[serde(default = "one")]
    #[schemars(range(min = 1))]
    pub expected_replacements: usize,
    pub expected_hash: Option<String>,
    #[schemars(description = OPTIONAL_HANDLE_DESCRIPTION)]
    pub handle: Option<String>,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub response_detail: ResponseDetail,
}

impl CodeReplaceRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        let change = Change::Replace {
            path: self.path,
            old_text: self.old_text,
            new_text: self.new_text,
            expected_replacements: self.expected_replacements,
            expected_hash: self.expected_hash,
            handle: self.handle,
        };
        EditRequest::single(change, self.validate, self.response_detail)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeReplaceRangeRequest {
    pub path: String,
    #[schemars(description = HANDLE_DESCRIPTION)]
    pub handle: String,
    #[schemars(description = REPLACEMENT_TEXT_DESCRIPTION)]
    pub new_text: String,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub response_detail: ResponseDetail,
}

impl CodeReplaceRangeRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        let change = Change::ReplaceRange {
            path: self.path,
            handle: self.handle,
            new_text: self.new_text,
        };
        EditRequest::single(change, self.validate, self.response_detail)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeInsertRequest {
    pub path: String,
    pub content: String,
    pub anchor_symbol: String,
    pub position: InsertPosition,
    pub expected_hash: Option<String>,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub response_detail: ResponseDetail,
}

impl CodeInsertRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        let change = Change::Insert {
            path: self.path,
            content: self.content,
            anchor_symbol: self.anchor_symbol,
            position: self.position,
            expected_hash: self.expected_hash,
        };
        EditRequest::single(change, self.validate, self.response_detail)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeDeleteRequest {
    pub path: String,
    pub expected_hash: Option<String>,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub response_detail: ResponseDetail,
}

impl CodeDeleteRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        let change = Change::Delete {
            path: self.path,
            expected_hash: self.expected_hash,
        };
        EditRequest::single(change, self.validate, self.response_detail)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeRenameRequest {
    pub path: String,
    pub to: String,
    pub expected_hash: Option<String>,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub response_detail: ResponseDetail,
}

impl CodeRenameRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        let change = Change::Rename {
            path: self.path,
            to: self.to,
            expected_hash: self.expected_hash,
        };
        EditRequest::single(change, self.validate, self.response_detail)
    }
}

// `changes` stays raw JSON until `Change::parse`, which reports each item with
// the change error codes; the schema publishes it as the typed union.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodePreviewRequest {
    #[schemars(with = "Vec<Change>")]
    pub changes: Vec<Value>,
    pub snapshot_id: Option<String>,
}

impl CodePreviewRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        EditRequest::batch(
            self.changes,
            self.snapshot_id,
            true,
            None,
            ResponseDetail::Standard,
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeTransactionRequest {
    #[schemars(with = "Vec<Change>")]
    pub changes: Vec<Value>,
    pub snapshot_id: Option<String>,
    #[schemars(description = VALIDATION_DESCRIPTION)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(description = TRANSACTION_DETAIL_DESCRIPTION)]
    pub response_detail: ResponseDetail,
}

impl CodeTransactionRequest {
    pub fn into_edit(self) -> AppResult<EditRequest> {
        EditRequest::batch(
            self.changes,
            self.snapshot_id,
            false,
            self.validate,
            self.response_detail,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_items_parse_into_typed_changes_with_defaults() {
        let change = Change::parse(json!({
            "kind": "replace",
            "path": "src/a.rs",
            "old_text": "old",
            "new_text": "new",
            "handle": null
        }))
        .unwrap();

        assert_eq!(
            change,
            Change::Replace {
                path: "src/a.rs".to_owned(),
                old_text: "old".to_owned(),
                new_text: "new".to_owned(),
                expected_replacements: 1,
                expected_hash: None,
                handle: None,
            }
        );
    }

    #[test]
    fn change_items_keep_their_error_codes() {
        let codes = [
            json!("not an object"),
            json!({"path": "a.rs"}),
            json!({"kind": "move", "path": "a.rs"}),
            json!({"kind": "rename", "path": "old.rs"}),
            json!({"kind": "delete", "path": "old.rs", "content": "x"}),
            json!({"kind": "insert", "path": "a.rs", "content": "x", "anchor_symbol": "f", "position": "middle"}),
            json!({"kind": "delete", "path": "  "}),
            json!({"kind": "replace_range", "path": "a.rs", "handle": "", "new_text": "x"}),
        ]
        .map(|value| Change::parse(value).unwrap_err().0.code);

        assert_eq!(
            codes,
            [
                "INVALID_CHANGE_FIELD",
                "MISSING_CHANGE_FIELD",
                "UNSUPPORTED_CHANGE_KIND",
                "MISSING_CHANGE_FIELD",
                "UNKNOWN_CHANGE_FIELD",
                "INVALID_CHANGE_FIELD",
                "INVALID_CHANGE_FIELD",
                "INVALID_CHANGE_FIELD",
            ]
        );
    }

    #[test]
    fn unsupported_kind_lists_the_supported_kinds() {
        let error = Change::parse(json!({"kind": "move", "path": "a.rs"})).unwrap_err();

        assert_eq!(
            error.0.details.unwrap()["supported"],
            json!([
                "create",
                "replace",
                "replace_range",
                "insert",
                "delete",
                "rename"
            ])
        );
    }

    #[test]
    fn narrow_write_defaults_to_overwrite() {
        let request: CodeWriteRequest = Contract::of::<CodeWriteRequest>("code_write")
            .parse(json!({"path": "a.txt", "content": "x"}), Rejection::Change)
            .unwrap();

        assert_eq!(
            request.into_edit().unwrap().changes,
            vec![Change::Create {
                path: "a.txt".to_owned(),
                content: "x".to_owned(),
                overwrite: true,
                expected_hash: None,
            }]
        );
    }
}
