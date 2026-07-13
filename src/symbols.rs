use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

thread_local! {
    static PARSERS: RefCell<HashMap<&'static str, Parser>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifierOccurrence {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub role: &'static str,
    pub evidence: &'static str,
    pub enclosing_symbol: Option<String>,
}

#[derive(Debug, Clone)]
struct SyntaxContext {
    role: &'static str,
    enclosing_symbol: Option<String>,
}

pub fn language_name(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "py" | "pyi" => "python",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "cs" => "csharp",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "json" => "json",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "log" => "log",
        _ => "text",
    }
}

fn language(path: &Path) -> Option<Language> {
    match language_name(path) {
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "csharp" => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        "c" => Some(tree_sitter_c::LANGUAGE.into()),
        "cpp" => Some(tree_sitter_cpp::LANGUAGE.into()),
        "json" => Some(tree_sitter_json::LANGUAGE.into()),
        _ => None,
    }
}

fn with_parser<T>(path: &Path, operation: impl FnOnce(&mut Parser) -> T) -> Option<T> {
    let name = language_name(path);
    let language = language(path)?;
    PARSERS.with(|pool| {
        let mut pool = pool.borrow_mut();
        if !pool.contains_key(name) {
            let mut parser = Parser::new();
            parser.set_language(&language).ok()?;
            pool.insert(name, parser);
        }
        let parser = pool.get_mut(name)?;
        parser.reset();
        Some(operation(parser))
    })
}

pub fn parse_has_error(path: &Path, content: &str) -> Option<bool> {
    with_parser(path, |parser| {
        parser
            .parse(content, None)
            .map(|tree| tree.root_node().has_error())
    })
    .flatten()
}

pub fn extract_symbols(path: &Path, content: &str) -> Vec<Symbol> {
    with_parser(path, |parser| {
        let Some(tree) = parser.parse(content, None) else {
            return Vec::new();
        };
        let mut output = Vec::new();
        walk(tree.root_node(), content.as_bytes(), &mut output);
        output
    })
    .unwrap_or_default()
}

/// Find every exact identifier occurrence. The lexical pass is the completeness
/// oracle; Tree-sitter enriches matching syntax nodes but never filters a hit.
pub fn identifier_occurrences(
    path: &Path,
    content: &str,
    identifier: &str,
) -> Vec<IdentifierOccurrence> {
    identifier_occurrences_with_symbols(path, content, identifier, &[])
}

/// Enrich exact occurrences using bounded syntax ranges selected from the
/// already-indexed symbol outline. This avoids reparsing a whole large file for
/// a handful of hits while keeping the lexical scan as the correctness oracle.
pub fn identifier_occurrences_with_symbols(
    path: &Path,
    content: &str,
    identifier: &str,
    symbols: &[Symbol],
) -> Vec<IdentifierOccurrence> {
    const MAX_SYNTAX_RANGE_LINES: usize = 200;
    const LOCAL_CONTEXT_LINES: usize = 20;

    let identifier = identifier.trim();
    if identifier.is_empty() {
        return Vec::new();
    }
    let mut occurrences = lexical_identifier_occurrences(content, identifier);
    if occurrences.is_empty() {
        return occurrences;
    }

    let line_starts = source_line_starts(content);
    let total_lines = line_starts.len().max(1);
    let mut ranges = BTreeMap::<(usize, usize), Option<String>>::new();
    if symbols.is_empty() {
        ranges.insert((1, total_lines), None);
    } else {
        for occurrence in &mut occurrences {
            let indexed_enclosing = symbols
                .iter()
                .filter(|symbol| {
                    symbol.start_line <= occurrence.start_line
                        && symbol.end_line >= occurrence.end_line
                })
                .min_by_key(|symbol| symbol.end_line.saturating_sub(symbol.start_line));
            let fallback_enclosing = indexed_enclosing
                .filter(|symbol| {
                    !(symbol.name == identifier && symbol.start_line == occurrence.start_line)
                })
                .map(|symbol| symbol.name.clone());
            occurrence.enclosing_symbol = fallback_enclosing.clone();

            let (mut start_line, mut end_line) = indexed_enclosing
                .map(|symbol| (symbol.start_line, symbol.end_line))
                .unwrap_or((occurrence.start_line, occurrence.end_line));
            if end_line.saturating_sub(start_line) + 1 > MAX_SYNTAX_RANGE_LINES {
                start_line = occurrence
                    .start_line
                    .saturating_sub(LOCAL_CONTEXT_LINES)
                    .max(1);
                end_line = (occurrence.end_line + LOCAL_CONTEXT_LINES).min(total_lines);
            }
            ranges
                .entry((start_line, end_line))
                .or_insert(fallback_enclosing);
        }
    }

    let mut syntax = HashMap::new();
    for ((start_line, end_line), fallback_enclosing) in ranges {
        let (start_byte, end_byte) =
            source_line_range_bytes(content, &line_starts, start_line, end_line);
        let source = &content[start_byte..end_byte];
        let contexts = with_parser(path, |parser| {
            parser.parse(source, None).map(|tree| {
                syntax_contexts(
                    tree.root_node(),
                    source.as_bytes(),
                    identifier,
                    start_byte,
                    fallback_enclosing.as_deref(),
                )
            })
        })
        .flatten();
        if let Some(contexts) = contexts {
            syntax.extend(contexts);
        }
    }

    for occurrence in &mut occurrences {
        if let Some(context) = syntax.get(&(occurrence.start_byte, occurrence.end_byte)) {
            occurrence.role = context.role;
            occurrence.evidence = "syntactic";
            if context.enclosing_symbol.is_some() {
                occurrence.enclosing_symbol = context.enclosing_symbol.clone();
            }
        }
    }
    occurrences
}

fn lexical_identifier_occurrences(content: &str, identifier: &str) -> Vec<IdentifierOccurrence> {
    let line_starts = source_line_starts(content);
    content
        .match_indices(identifier)
        .filter_map(|(start_byte, _)| {
            let end_byte = start_byte + identifier.len();
            let before = content[..start_byte].chars().next_back();
            let after = content[end_byte..].chars().next();
            if before.is_some_and(is_identifier_continue)
                || after.is_some_and(is_identifier_continue)
            {
                return None;
            }
            let (start_line, start_column) = source_position(content, &line_starts, start_byte);
            let (end_line, end_column) = source_position(content, &line_starts, end_byte);
            Some(IdentifierOccurrence {
                start_byte,
                end_byte,
                start_line,
                start_column,
                end_line,
                end_column,
                role: "other",
                evidence: "lexical",
                enclosing_symbol: None,
            })
        })
        .collect()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_'
        || character == '$'
        || character.is_alphanumeric()
        || matches!(character, '\u{200c}' | '\u{200d}')
        || ('\u{0300}'..='\u{036f}').contains(&character)
        || ('\u{1ab0}'..='\u{1aff}').contains(&character)
        || ('\u{1dc0}'..='\u{1dff}').contains(&character)
        || ('\u{20d0}'..='\u{20ff}').contains(&character)
        || ('\u{fe20}'..='\u{fe2f}').contains(&character)
}

fn source_line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(content.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

fn source_position(content: &str, line_starts: &[usize], byte: usize) -> (usize, usize) {
    let line_index = line_starts
        .partition_point(|line_start| *line_start <= byte)
        .saturating_sub(1);
    let line_start = line_starts[line_index];
    let column = content[line_start..byte].chars().count() + 1;
    (line_index + 1, column)
}

fn source_line_range_bytes(
    content: &str,
    line_starts: &[usize],
    start_line: usize,
    end_line: usize,
) -> (usize, usize) {
    let start_index = start_line.saturating_sub(1).min(line_starts.len() - 1);
    let end_index = end_line.min(line_starts.len());
    let start_byte = line_starts[start_index];
    let end_byte = line_starts.get(end_index).copied().unwrap_or(content.len());
    (start_byte, end_byte)
}

fn syntax_contexts(
    root: Node<'_>,
    source: &[u8],
    identifier: &str,
    base_byte: usize,
    fallback_enclosing: Option<&str>,
) -> HashMap<(usize, usize), SyntaxContext> {
    let mut contexts = HashMap::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_identifier_node(node) && node.utf8_text(source).ok() == Some(identifier) {
            contexts.insert(
                (base_byte + node.start_byte(), base_byte + node.end_byte()),
                SyntaxContext {
                    role: classify_identifier_node(node),
                    enclosing_symbol: enclosing_symbol_name(node, source)
                        .or_else(|| fallback_enclosing.map(str::to_owned)),
                },
            );
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    contexts
}

fn is_identifier_node(node: Node<'_>) -> bool {
    if !node.is_named() || node.named_child_count() != 0 {
        return false;
    }
    let kind = node.kind();
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "type_identifier"
            | "namespace_identifier"
            | "name"
    ) || kind.ends_with("_identifier")
}

fn classify_identifier_node(node: Node<'_>) -> &'static str {
    if is_declaration_name(node) {
        return "declaration";
    }
    if node.kind().contains("type_identifier") {
        return "type";
    }
    let mut current = node.parent();
    while let Some(parent) = current {
        let kind = parent.kind();
        if is_import_context(kind) {
            return "import";
        }
        if is_write_context(parent, node) {
            return "write";
        }
        if is_call_context(kind) {
            return "call";
        }
        if is_type_context(kind) {
            return "type";
        }
        current = parent.parent();
    }
    "read"
}

fn is_declaration_name(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_occurrence_declaration(parent.kind()) {
            if let Some(name) = parent
                .child_by_field_name("name")
                .or_else(|| find_identifier(parent))
            {
                if same_node(name, node) {
                    return true;
                }
            }
        }
        current = parent.parent();
    }
    false
}

fn is_import_context(kind: &str) -> bool {
    kind.contains("import")
        || matches!(
            kind,
            "use_declaration" | "include_directive" | "preproc_include" | "using_directive"
        )
}

fn is_call_context(kind: &str) -> bool {
    kind.contains("call")
        || matches!(
            kind,
            "method_invocation" | "invocation_expression" | "macro_invocation" | "command"
        )
}

fn is_write_context(parent: Node<'_>, node: Node<'_>) -> bool {
    let kind = parent.kind();
    if matches!(kind, "update_expression" | "postfix_unary_expression") {
        return contains_node(parent, node);
    }
    if kind.contains("assignment") || matches!(kind, "short_var_declaration") {
        return parent
            .child_by_field_name("left")
            .or_else(|| parent.child_by_field_name("name"))
            .is_some_and(|left| contains_node(left, node));
    }
    false
}

fn is_type_context(kind: &str) -> bool {
    kind.contains("type")
        || matches!(
            kind,
            "cast_expression" | "generic_parameter" | "implements_clause" | "extends_clause"
        )
}

fn contains_node(container: Node<'_>, node: Node<'_>) -> bool {
    container.start_byte() <= node.start_byte() && container.end_byte() >= node.end_byte()
}

fn same_node(left: Node<'_>, right: Node<'_>) -> bool {
    left.start_byte() == right.start_byte() && left.end_byte() == right.end_byte()
}

fn enclosing_symbol_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_declaration(parent.kind()) {
            if let Some(name) = parent
                .child_by_field_name("name")
                .or_else(|| find_identifier(parent))
            {
                if !same_node(name, node) {
                    if let Ok(name) = name.utf8_text(source) {
                        return Some(name.to_owned());
                    }
                }
            }
        }
        current = parent.parent();
    }
    None
}

fn walk(root: Node<'_>, source: &[u8], output: &mut Vec<Symbol>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_declaration(node.kind()) {
            let name_node = node
                .child_by_field_name("name")
                .or_else(|| find_identifier(node));
            if let Some(name_node) = name_node {
                if let Ok(name) = name_node.utf8_text(source) {
                    let first_line = node.start_position().row + 1;
                    let last_line = node.end_position().row + 1;
                    let signature = node
                        .utf8_text(source)
                        .unwrap_or_default()
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .chars()
                        .take(300)
                        .collect();
                    output.push(Symbol {
                        name: name.to_owned(),
                        kind: node.kind().to_owned(),
                        start_line: first_line,
                        end_line: last_line,
                        signature,
                    });
                }
            }
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
}

fn find_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "identifier"
                | "field_identifier"
                | "property_identifier"
                | "type_identifier"
                | "namespace_identifier"
                | "name"
        )
    });
    found
}

fn is_occurrence_declaration(kind: &str) -> bool {
    is_declaration(kind)
        || matches!(
            kind,
            "variable_declarator"
                | "lexical_declaration"
                | "let_declaration"
                | "const_item"
                | "static_item"
                | "field_declaration"
                | "parameter"
                | "required_parameter"
                | "optional_parameter"
                | "formal_parameter"
        )
}

fn is_declaration(kind: &str) -> bool {
    matches!(
        kind,
        "function_definition"
            | "class_definition"
            | "decorated_definition"
            | "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "type_item"
            | "mod_item"
            | "method_declaration"
            | "type_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "namespace_declaration"
            | "struct_specifier"
            | "enum_specifier"
            | "union_specifier"
            | "template_declaration"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_function() {
        let symbols = extract_symbols(Path::new("x.rs"), "pub fn hello() {}\n");
        assert!(symbols.iter().any(|symbol| symbol.name == "hello"));
    }

    #[test]
    fn exact_identifier_boundaries_exclude_identifier_substrings() {
        let content = "run run_more prerun $run run();\n";
        let occurrences = identifier_occurrences(Path::new("x.rs"), content, "run");
        assert_eq!(occurrences.len(), 2);
        assert_eq!(occurrences[0].start_column, 1);
        assert_eq!(occurrences[1].role, "call");
    }

    #[test]
    fn receiver_qualified_call_has_syntactic_role_and_enclosing_symbol() {
        let content = "impl Validator {\n    fn apply(&self) {\n        self.run_edit_validation();\n    }\n}\n";
        let occurrences =
            identifier_occurrences(Path::new("validator.rs"), content, "run_edit_validation");
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].role, "call");
        assert_eq!(occurrences[0].evidence, "syntactic");
        assert_eq!(occurrences[0].enclosing_symbol.as_deref(), Some("apply"));
    }

    #[test]
    fn tree_sitter_classifies_python_import_and_typescript_write() {
        let imported = identifier_occurrences(
            Path::new("module.py"),
            "from owner import extract\n",
            "extract",
        );
        assert_eq!(imported[0].role, "import");
        assert_eq!(imported[0].evidence, "syntactic");

        let written = identifier_occurrences(
            Path::new("module.ts"),
            "function update() { value = 1; }\n",
            "value",
        );
        assert_eq!(written[0].role, "write");
        assert_eq!(written[0].enclosing_symbol.as_deref(), Some("update"));
    }
}
