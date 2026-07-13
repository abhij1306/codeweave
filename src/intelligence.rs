//! Manager-owned intelligence boundary. It never mutates workspace files: a
//! rename request can only return previewable `changes[]` once a semantic LSP
//! backend supplies an edit. Tree-sitter and lexical scans remain explicit
//! fallbacks while language servers are unavailable.

use crate::model::{AppError, AppResult, IntelligenceSettings};
use crate::process_runtime::JsonRpcProcess;
use crate::security::validate_relative;
use crate::symbols::{extract_symbols, language_name, parse_has_error};
use ignore::WalkBuilder;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct IntelligenceService {
    root: PathBuf,
    settings: IntelligenceSettings,
    python: Arc<LspClient>,
    typescript: Arc<LspClient>,
}

struct LspState {
    process: Option<JsonRpcProcess>,
    next_id: u64,
    restarts: u64,
    last_error: Option<String>,
    opened: HashSet<PathBuf>,
}
struct LspClient {
    root: PathBuf,
    language: &'static str,
    settings: crate::model::LanguageServerSettings,
    state: Mutex<LspState>,
}

impl LspClient {
    fn new(
        root: PathBuf,
        language: &'static str,
        settings: crate::model::LanguageServerSettings,
    ) -> Self {
        Self {
            root,
            language,
            settings,
            state: Mutex::new(LspState {
                process: None,
                next_id: 1,
                restarts: 0,
                last_error: None,
                opened: HashSet::new(),
            }),
        }
    }
    fn configured(&self) -> bool {
        self.settings.enabled && !self.settings.command.is_empty()
    }
    fn request(&self, method: &str, params: Value) -> AppResult<Value> {
        if !self.configured() {
            return Err(AppError::new(
                "SEMANTIC_BACKEND_UNAVAILABLE",
                format!("{} LSP is disabled", self.language),
            ));
        }
        let mut state = self.state.lock();
        for attempt in 0..2 {
            if state.process.is_none() {
                self.start(&mut state)?;
            }
            let id = state.next_id;
            state.next_id += 1;
            let process = state.process.as_mut().expect("started");
            process
                .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .map_err(AppError::internal)?;
            let deadline =
                std::time::Instant::now() + Duration::from_millis(self.settings.timeout_ms);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match process.receive(remaining) {
                    Ok(message) if message.get("id").and_then(Value::as_u64) == Some(id) => {
                        if let Some(error) = message.get("error") {
                            return Err(AppError::details(
                                "LSP_ERROR",
                                format!("{} LSP request failed", self.language),
                                error.clone(),
                            ));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        state.last_error = Some(error.to_string());
                        state.process = None;
                        state.opened.clear();
                        if attempt == 0 {
                            state.restarts += 1;
                            break;
                        }
                        return Err(AppError::details(
                            "LSP_TIMEOUT",
                            error.to_string(),
                            json!({"language":self.language,"method":method}),
                        ));
                    }
                }
            }
        }
        unreachable!()
    }
    fn notify(&self, method: &str, params: Value) -> AppResult<()> {
        let mut state = self.state.lock();
        if state.process.is_none() {
            self.start(&mut state)?;
        }
        state
            .process
            .as_mut()
            .expect("started")
            .send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .map_err(AppError::internal)
    }
    fn open_document(&self, path: &Path) -> AppResult<()> {
        if self.state.lock().opened.contains(path) {
            return Ok(());
        }
        let content = fs::read_to_string(path)?;
        self.notify("textDocument/didOpen",json!({"textDocument":{"uri":path_uri(path),"languageId":self.language,"version":1,"text":content}}))?;
        self.state.lock().opened.insert(path.to_owned());
        Ok(())
    }
    fn diagnostics(&self, path: &Path) -> AppResult<Value> {
        self.open_document(path)?;
        let state = self.state.lock();
        let process = state.process.as_ref().expect("opened");
        let timeout = Duration::from_millis(self.settings.timeout_ms);
        loop {
            let value = process.receive(timeout).map_err(AppError::internal)?;
            if value.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
                && value["params"]["uri"].as_str() == Some(path_uri(path).as_str())
            {
                return Ok(value["params"]["diagnostics"].clone());
            }
        }
    }
    fn start(&self, state: &mut LspState) -> AppResult<()> {
        let mut process =
            JsonRpcProcess::spawn(&self.settings.command, &self.settings.args, &self.root)
                .map_err(|error| {
                    AppError::details(
                        "LSP_START_FAILED",
                        error.to_string(),
                        json!({"language":self.language,"command":self.settings.command}),
                    )
                })?;
        let id = state.next_id;
        state.next_id += 1;
        process.send(&json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"processId":std::process::id(),"rootUri":path_uri(&self.root),"capabilities":{"textDocument":{"publishDiagnostics":{},"definition":{},"references":{},"rename":{}}},"clientInfo":{"name":"CodeWeave","version":env!("CARGO_PKG_VERSION")}}})).map_err(AppError::internal)?;
        let timeout = Duration::from_millis(self.settings.timeout_ms);
        loop {
            let message = process.receive(timeout).map_err(AppError::internal)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                break;
            }
        }
        process
            .send(&json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
            .map_err(AppError::internal)?;
        state.process = Some(process);
        state.last_error = None;
        Ok(())
    }
    fn status(&self) -> Value {
        let state = self.state.lock();
        json!({"configured":self.configured(),"readiness":if state.process.is_some(){"ready"}else if self.configured(){"lazy"}else{"disabled"},"restart_count":state.restarts,"last_error":state.last_error})
    }
}

fn path_uri(path: &Path) -> String {
    format!(
        "file:///{}",
        path.to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
            .replace(' ', "%20")
    )
}
fn uri_path(root: &Path, uri: &str) -> AppResult<PathBuf> {
    let raw = uri
        .strip_prefix("file:///")
        .ok_or_else(|| AppError::new("UNSUPPORTED_LSP_URI", "LSP returned a non-file URI"))?
        .replace("%20", " ");
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        PathBuf::from(format!("/{}", path.display()))
    };
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(AppError::new(
            "OUTSIDE_ROOT",
            "LSP result is outside the workspace",
        ));
    }
    Ok(canonical)
}

pub trait CodeIntelligenceBackend: Send + Sync {
    fn definition(&self, path: &Path, line: usize, column: usize) -> AppResult<Value>;
    fn diagnostics(&self, path: &Path) -> AppResult<Value>;
}

#[derive(Default)]
pub struct TreeSitterBackend;

impl CodeIntelligenceBackend for TreeSitterBackend {
    fn definition(&self, path: &Path, line: usize, _column: usize) -> AppResult<Value> {
        let content = fs::read_to_string(path)?;
        let symbol = extract_symbols(path, &content)
            .into_iter()
            .find(|symbol| symbol.start_line <= line && symbol.end_line >= line);
        Ok(json!({
            "evidence": "syntactic",
            "result": symbol.map(|symbol| json!({
                "path": path,
                "symbol": symbol,
            }))
        }))
    }

    fn diagnostics(&self, path: &Path) -> AppResult<Value> {
        let content = fs::read_to_string(path)?;
        Ok(json!({
            "evidence": "syntactic",
            "diagnostics": if parse_has_error(path, &content) == Some(true) {
                vec![json!({"severity": "error", "message": "Tree-sitter reported a syntax error"})]
            } else { Vec::<Value>::new() }
        }))
    }
}

impl IntelligenceService {
    pub fn new(root: PathBuf, settings: IntelligenceSettings) -> Self {
        let python = Arc::new(LspClient::new(
            root.clone(),
            "python",
            settings.python.clone(),
        ));
        let typescript = Arc::new(LspClient::new(
            root.clone(),
            "typescript",
            settings.typescript.clone(),
        ));
        Self {
            root,
            settings,
            python,
            typescript,
        }
    }

    pub fn capabilities(&self) -> Value {
        let language = |name: &str,
                        configured: &crate::model::LanguageServerSettings,
                        default_command: &str| {
            json!({
                "language": name,
                "configured": configured.enabled,
                "command": if configured.command.is_empty() { default_command } else { configured.command.as_str() },
                "args": configured.args,
                "timeout_ms": configured.timeout_ms,
                "readiness": if configured.enabled { "lazy" } else { "disabled" },
                "fallback": "tree_sitter_then_lexical"
            })
        };
        json!({
            "supported_operations": ["definition", "references", "diagnostics", "rename_preview"],
            "languages": [
                language("python", &self.settings.python, "basedpyright-langserver"),
                language("typescript", &self.settings.typescript, "typescript-language-server")
            ],
            "restart_state": {"python":self.python.status(),"typescript":self.typescript.status()},
            "semantic_available": self.python.configured() || self.typescript.configured(),
            "fallback_status": "tree_sitter_then_lexical"
        })
    }

    pub fn execute(&self, params: &Value) -> AppResult<Value> {
        let operation = params
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("code_intelligence requires operation"))?;
        match operation {
            "definition" => self.definition(params),
            "references" => self.references(params),
            "diagnostics" => self.diagnostics(params),
            "rename_preview" => self.rename_preview(params),
            _ => Err(AppError::invalid("Unknown code_intelligence operation")),
        }
    }

    fn local_path(&self, params: &Value) -> AppResult<PathBuf> {
        let relative = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("code_intelligence requires path"))?;
        Ok(self.root.join(validate_relative(relative)?))
    }

    fn definition(&self, params: &Value) -> AppResult<Value> {
        let path = self.local_path(params)?;
        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("definition requires line"))?
            as usize;
        let column = params.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
        if let Some(client) = self.client_for(&path) {
            let _ = client.open_document(&path);
            match client.request(
                "textDocument/definition",
                position_params(&path, line, column),
            ) {
                Ok(result) => {
                    return Ok(
                        json!({"operation":"definition","semantic":true,"evidence":"semantic","results":normalize_locations(&self.root,&result)?}),
                    )
                }
                Err(error) => {
                    let fallback = TreeSitterBackend.definition(&path, line, column)?;
                    return Ok(
                        json!({"operation":"definition","semantic":false,"evidence":"syntactic","fallback_reason":error.0,"result":fallback}),
                    );
                }
            }
        }
        let result = TreeSitterBackend.definition(&path, line, column)?;
        Ok(
            json!({"operation":"definition","semantic":false,"evidence":"syntactic","result":result}),
        )
    }

    fn diagnostics(&self, params: &Value) -> AppResult<Value> {
        let path = self.local_path(params)?;
        if let Some(client) = self.client_for(&path) {
            match client.diagnostics(&path) {
                Ok(diagnostics) => {
                    return Ok(
                        json!({"operation":"diagnostics","semantic":true,"evidence":"semantic","diagnostics":diagnostics}),
                    )
                }
                Err(error) => {
                    let result = TreeSitterBackend.diagnostics(&path)?;
                    return Ok(
                        json!({"operation":"diagnostics","semantic":false,"evidence":"syntactic","fallback_reason":error.0,"result":result}),
                    );
                }
            }
        }
        let result = TreeSitterBackend.diagnostics(&path)?;
        Ok(json!({"operation": "diagnostics", "semantic": false, "result": result}))
    }

    fn references(&self, params: &Value) -> AppResult<Value> {
        let path = self.local_path(params)?;
        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("references requires line"))?
            as usize;
        let column = params.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
        if let Some(client) = self.client_for(&path) {
            let _ = client.open_document(&path);
            let mut request = position_params(&path, line, column);
            request["context"] = json!({"includeDeclaration":false});
            if let Ok(result) = client.request("textDocument/references", request) {
                let mut locations = normalize_locations(&self.root, &result)?;
                locations.truncate(
                    params
                        .get("max_results")
                        .and_then(Value::as_u64)
                        .unwrap_or(20)
                        .min(200) as usize,
                );
                return Ok(
                    json!({"operation":"references","semantic":true,"evidence":"semantic","results":locations}),
                );
            }
        }
        let content = fs::read_to_string(&path)?;
        let symbol = extract_symbols(&path, &content)
            .into_iter()
            .find(|symbol| symbol.start_line <= line && symbol.end_line >= line)
            .ok_or_else(|| {
                AppError::new(
                    "SYMBOL_NOT_FOUND",
                    "No indexed declaration contains the requested position",
                )
            })?;
        let identifier = Regex::new(&format!(r"\b{}\b", regex::escape(&symbol.name)))
            .map_err(AppError::internal)?;
        let max_results = params
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .min(200) as usize;
        let mut results = Vec::new();
        for entry in WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .build()
            .flatten()
        {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let candidate = entry.path();
            let Ok(source) = fs::read_to_string(candidate) else {
                continue;
            };
            for (index, text) in source.lines().enumerate() {
                if candidate == path && index + 1 == symbol.start_line {
                    continue;
                }
                if identifier.is_match(text) {
                    let relative = candidate
                        .strip_prefix(&self.root)
                        .unwrap_or(candidate)
                        .to_string_lossy()
                        .replace('\\', "/");
                    results.push(json!({
                        "path": relative,
                        "line": index + 1,
                        "preview": text,
                        "evidence": "lexical",
                        "classification_evidence": if language_name(candidate) == "text" { "lexical" } else { "syntactic" },
                        "reference_kind": "other"
                    }));
                    if results.len() >= max_results {
                        break;
                    }
                }
            }
            if results.len() >= max_results {
                break;
            }
        }
        Ok(json!({
            "operation": "references",
            "symbol": symbol,
            "evidence": "lexical",
            "semantic": false,
            "results": results,
            "capabilities": self.capabilities()
        }))
    }

    fn client_for(&self, path: &Path) -> Option<&LspClient> {
        match language_name(path) {
            "python" => self.python.configured().then_some(self.python.as_ref()),
            "typescript" | "tsx" | "javascript" => self
                .typescript
                .configured()
                .then_some(self.typescript.as_ref()),
            _ => None,
        }
    }

    fn rename_preview(&self, params: &Value) -> AppResult<Value> {
        let path = self.local_path(params)?;
        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("rename_preview requires line"))?
            as usize;
        let column = params.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
        let new_name = params
            .get("new_name")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("rename_preview requires new_name"))?;
        let client = self.client_for(&path).ok_or_else(|| {
            AppError::new(
                "SEMANTIC_BACKEND_UNAVAILABLE",
                "No enabled LSP backend supports this file",
            )
        })?;
        client.open_document(&path)?;
        let mut request = position_params(&path, line, column);
        request["newName"] = json!(new_name);
        let edit = client.request("textDocument/rename", request)?;
        let changes = workspace_edit_changes(&self.root, &edit)?;
        Ok(
            json!({"operation":"rename_preview","semantic":true,"evidence":"semantic","changes":changes}),
        )
    }
}

fn position_params(path: &Path, line: usize, column: usize) -> Value {
    json!({"textDocument":{"uri":path_uri(path)},"position":{"line":line.saturating_sub(1),"character":column}})
}
fn normalize_locations(root: &Path, value: &Value) -> AppResult<Vec<Value>> {
    let items = if let Some(array) = value.as_array() {
        array.clone()
    } else if value.is_null() {
        vec![]
    } else {
        vec![value.clone()]
    };
    items.into_iter().map(|item|{let uri=item.get("uri").or_else(||item.get("targetUri")).and_then(Value::as_str).ok_or_else(||AppError::new("INVALID_LSP_RESPONSE","Location lacks URI"))?;let range=item.get("range").or_else(||item.get("targetSelectionRange")).ok_or_else(||AppError::new("INVALID_LSP_RESPONSE","Location lacks range"))?;let path=uri_path(root,uri)?;let relative=path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\',"/");Ok(json!({"path":relative,"line":range["start"]["line"].as_u64().unwrap_or(0)+1,"column":range["start"]["character"],"end_line":range["end"]["line"].as_u64().unwrap_or(0)+1,"end_column":range["end"]["character"],"evidence":"semantic"}))}).collect()
}

fn utf16_offset(content: &str, line: u64, character: u64) -> AppResult<usize> {
    let mut base = 0;
    let text = content
        .split_inclusive('\n')
        .nth(line as usize)
        .ok_or_else(|| AppError::new("INVALID_LSP_EDIT", "Edit line is outside file"))?;
    for previous in content.split_inclusive('\n').take(line as usize) {
        base += previous.len();
    }
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= character {
            return Ok(base + index);
        }
        units += ch.len_utf16() as u64;
        if units > character {
            return Err(AppError::new(
                "INVALID_LSP_EDIT",
                "Edit splits a UTF-16 code point",
            ));
        }
    }
    if units == character {
        Ok(base + text.len())
    } else {
        Err(AppError::new(
            "INVALID_LSP_EDIT",
            "Edit character is outside line",
        ))
    }
}
fn workspace_edit_changes(root: &Path, edit: &Value) -> AppResult<Vec<Value>> {
    let mut documents: Vec<(String, Value)> = Vec::new();
    if let Some(object) = edit.get("changes").and_then(Value::as_object) {
        documents.extend(
            object
                .iter()
                .map(|(uri, edits)| (uri.clone(), edits.clone())),
        );
    }
    if let Some(items) = edit.get("documentChanges").and_then(Value::as_array) {
        for item in items {
            if item.get("kind").is_some() {
                return Err(AppError::new(
                    "UNSUPPORTED_WORKSPACE_EDIT",
                    "LSP resource create/rename/delete operations are not supported",
                ));
            }
            let uri = item["textDocument"]["uri"]
                .as_str()
                .ok_or_else(|| AppError::new("INVALID_LSP_EDIT", "TextDocumentEdit lacks a URI"))?;
            documents.push((uri.to_owned(), item["edits"].clone()));
        }
    }
    if documents.is_empty() {
        return Err(AppError::new(
            "UNSUPPORTED_WORKSPACE_EDIT",
            "WorkspaceEdit contains no supported text edits",
        ));
    }
    let mut output = Vec::new();
    for (uri, edits) in documents {
        let path = uri_path(root, &uri)?;
        let before = fs::read_to_string(&path)?;
        let mut ranges = Vec::new();
        for item in edits
            .as_array()
            .ok_or_else(|| AppError::new("INVALID_LSP_EDIT", "Edits must be an array"))?
        {
            let range = &item["range"];
            let start = utf16_offset(
                &before,
                range["start"]["line"].as_u64().unwrap_or(0),
                range["start"]["character"].as_u64().unwrap_or(0),
            )?;
            let end = utf16_offset(
                &before,
                range["end"]["line"].as_u64().unwrap_or(0),
                range["end"]["character"].as_u64().unwrap_or(0),
            )?;
            ranges.push((
                start,
                end,
                item["newText"].as_str().unwrap_or("").to_owned(),
            ));
        }
        ranges.sort_by_key(|range| range.0);
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(AppError::new(
                "OVERLAPPING_LSP_EDITS",
                "LSP returned overlapping edits",
            ));
        }
        let mut after = before.clone();
        for (start, end, text) in ranges.into_iter().rev() {
            after.replace_range(start..end, &text);
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        output.push(json!({"kind":"replace","path":relative,"old_text":before,"new_text":after,"expected_replacements":1,"expected_hash":crate::index::content_hash(&fs::read_to_string(&path)?)}));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LanguageServerSettings;

    #[test]
    fn configured_lsp_smoke_from_environment() {
        let (Ok(root), Ok(command), Ok(file)) = (
            std::env::var("CODEWEAVE_LSP_TEST_ROOT"),
            std::env::var("CODEWEAVE_LSP_TEST_COMMAND"),
            std::env::var("CODEWEAVE_LSP_TEST_FILE"),
        ) else {
            return;
        };
        let root = PathBuf::from(root);
        let path = root.join(file);
        let client = LspClient::new(
            root,
            "python",
            LanguageServerSettings {
                enabled: true,
                command,
                args: vec!["--stdio".into()],
                timeout_ms: 30_000,
            },
        );
        client
            .open_document(&path)
            .expect("LSP initializes and opens a document");
        client
            .request("textDocument/definition", position_params(&path, 1, 0))
            .expect("LSP answers a definition request");
        let mut rename = position_params(&path, 64, 5);
        rename["newName"] = json!("extract_codeweave_smoke");
        let edit = client
            .request("textDocument/rename", rename)
            .expect("LSP answers a rename request");
        eprintln!("workspace edit: {edit}");
        let changes = workspace_edit_changes(&client.root, &edit)
            .expect("LSP rename compiles to transaction changes");
        assert!(!changes.is_empty());
        assert_eq!(client.status()["readiness"], "ready");
    }

    #[test]
    fn workspace_edit_compiles_utf16_ranges_to_transaction_changes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sample.py");
        fs::write(&path, "def café():\n    return 1\n").unwrap();
        let edit = json!({"changes":{path_uri(&path):[{"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":8}},"newText":"bistro"}]}});
        let changes = workspace_edit_changes(root.path(), &edit).unwrap();
        assert_eq!(changes[0]["kind"], "replace");
        assert!(changes[0]["new_text"].as_str().unwrap().contains("bistro"));
    }
}
