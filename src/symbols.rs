use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
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
    let found = node
        .children(&mut cursor)
        .find(|child| matches!(child.kind(), "identifier" | "type_identifier" | "name"));
    found
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
        assert!(symbols.iter().any(|s| s.name == "hello"));
    }
}
