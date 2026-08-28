use std::path::Path;

use once_cell::sync::Lazy;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

use super::types::expand_tabs;

/// Configuration for context lines feature
#[derive(Clone)]
pub struct ContextConfig {
    pub enabled: bool,
    pub max_lines: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_lines: 5,
        }
    }
}

/// Represents a context line with its content and original line number
#[derive(Clone, Debug)]
pub struct ContextLine {
    pub line_number: usize,
    pub content: String,
}

// Context queries for each supported language
// These define which AST nodes should be shown as context

const RUST_CONTEXT_QUERY: &str = r#"
(function_item) @context
(impl_item) @context
(struct_item) @context
(enum_item) @context
(trait_item) @context
(mod_item) @context
(match_expression) @context
(for_expression) @context
(while_expression) @context
(loop_expression) @context
(if_expression) @context
(closure_expression) @context
"#;

const TYPESCRIPT_CONTEXT_QUERY: &str = r#"
(function_declaration) @context
(method_definition) @context
(class_declaration) @context
(interface_declaration) @context
(for_statement) @context
(for_in_statement) @context
(while_statement) @context
(do_statement) @context
(if_statement) @context
(switch_statement) @context
(try_statement) @context
(arrow_function) @context
(function_expression) @context
"#;

const JAVASCRIPT_CONTEXT_QUERY: &str = r#"
(function_declaration) @context
(method_definition) @context
(class_declaration) @context
(for_statement) @context
(for_in_statement) @context
(while_statement) @context
(do_statement) @context
(if_statement) @context
(switch_statement) @context
(try_statement) @context
(arrow_function) @context
(function_expression) @context
"#;

const PYTHON_CONTEXT_QUERY: &str = r#"
(function_definition) @context
(class_definition) @context
(for_statement) @context
(while_statement) @context
(if_statement) @context
(try_statement) @context
(with_statement) @context
(match_statement) @context
"#;

const GO_CONTEXT_QUERY: &str = r#"
(function_declaration) @context
(method_declaration) @context
(type_declaration) @context
(for_statement) @context
(if_statement) @context
(switch_statement) @context
(select_statement) @context
"#;

const CSHARP_CONTEXT_QUERY: &str = r#"
(namespace_declaration) @context
(class_declaration) @context
(struct_declaration) @context
(interface_declaration) @context
(enum_declaration) @context
(record_declaration) @context
(method_declaration) @context
(constructor_declaration) @context
(property_declaration) @context
(for_statement) @context
(foreach_statement) @context
(while_statement) @context
(do_statement) @context
(if_statement) @context
(switch_statement) @context
(try_statement) @context
(using_statement) @context
(lock_statement) @context
"#;

struct LanguageContext {
    language: Language,
    query: Query,
}

static LANGUAGE_CONTEXTS: Lazy<Vec<(&'static str, LanguageContext)>> = Lazy::new(|| {
    let mut contexts = Vec::new();

    // Rust
    let rust_lang: Language = tree_sitter_rust::LANGUAGE.into();
    if let Ok(query) = Query::new(&rust_lang, RUST_CONTEXT_QUERY) {
        contexts.push((
            "rs",
            LanguageContext {
                language: rust_lang,
                query,
            },
        ));
    }

    // TypeScript
    let ts_lang: Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    if let Ok(query) = Query::new(&ts_lang, TYPESCRIPT_CONTEXT_QUERY) {
        contexts.push((
            "ts",
            LanguageContext {
                language: ts_lang,
                query,
            },
        ));
    }

    // TSX
    let tsx_lang: Language = tree_sitter_typescript::LANGUAGE_TSX.into();
    if let Ok(query) = Query::new(&tsx_lang, TYPESCRIPT_CONTEXT_QUERY) {
        contexts.push((
            "tsx",
            LanguageContext {
                language: tsx_lang,
                query,
            },
        ));
    }

    // JavaScript
    let js_lang: Language = tree_sitter_javascript::LANGUAGE.into();
    if let Ok(query) = Query::new(&js_lang, JAVASCRIPT_CONTEXT_QUERY) {
        contexts.push((
            "js",
            LanguageContext {
                language: js_lang.clone(),
                query,
            },
        ));
    }

    // JSX (same as JS)
    if let Ok(query) = Query::new(&js_lang, JAVASCRIPT_CONTEXT_QUERY) {
        contexts.push((
            "jsx",
            LanguageContext {
                language: js_lang,
                query,
            },
        ));
    }

    // Python
    let py_lang: Language = tree_sitter_python::LANGUAGE.into();
    if let Ok(query) = Query::new(&py_lang, PYTHON_CONTEXT_QUERY) {
        contexts.push((
            "py",
            LanguageContext {
                language: py_lang,
                query,
            },
        ));
    }

    // Go
    let go_lang: Language = tree_sitter_go::LANGUAGE.into();
    if let Ok(query) = Query::new(&go_lang, GO_CONTEXT_QUERY) {
        contexts.push((
            "go",
            LanguageContext {
                language: go_lang,
                query,
            },
        ));
    }

    // C#
    let csharp_lang: Language = tree_sitter_c_sharp::LANGUAGE.into();
    if let Ok(query) = Query::new(&csharp_lang, CSHARP_CONTEXT_QUERY) {
        contexts.push((
            "cs",
            LanguageContext {
                language: csharp_lang,
                query,
            },
        ));
    }

    contexts
});

fn get_language_context(filename: &str) -> Option<&'static LanguageContext> {
    let ext = Path::new(filename).extension().and_then(|e| e.to_str())?;
    LANGUAGE_CONTEXTS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, ctx)| ctx)
}

/// A context-worthy node collected from the AST. Scroll-position-independent, so
/// it is cached per-file in `AppState` and reused across renders.
#[derive(Clone)]
pub struct ContextNode {
    start_line: usize,
    end_line: usize,
    content: String,
}

/// Parse the source and collect every context-worthy node (sorted by start line, deduplicated).
/// Does not depend on scroll position — the result is cacheable for the lifetime of the source.
pub fn collect_context_nodes(source: &str, filename: &str, tab_width: usize) -> Vec<ContextNode> {
    let Some(lang_ctx) = get_language_context(filename) else {
        return Vec::new();
    };

    let mut parser = Parser::new();
    if parser.set_language(&lang_ctx.language).is_err() {
        return Vec::new();
    }

    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut cursor = QueryCursor::new();
    let lines: Vec<&str> = source.lines().collect();

    let mut nodes: Vec<ContextNode> = Vec::new();
    let mut matches = cursor.matches(&lang_ctx.query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let start_line = node.start_position().row;
            let end_line = node.end_position().row;
            if let Some(line_content) = lines.get(start_line) {
                nodes.push(ContextNode {
                    start_line,
                    end_line,
                    content: expand_tabs(line_content, tab_width),
                });
            }
        }
    }

    nodes.sort_by_key(|n| n.start_line);
    nodes.dedup_by_key(|n| n.start_line);
    nodes
}

fn filter_to_lines(
    nodes: &[ContextNode],
    scroll_position: usize,
    max_lines: usize,
) -> Vec<ContextLine> {
    nodes
        .iter()
        .filter(|n| n.start_line < scroll_position && n.end_line >= scroll_position)
        .take(max_lines)
        .map(|n| ContextLine {
            line_number: n.start_line + 1,
            content: n.content.clone(),
        })
        .collect()
}

/// Turn pre-collected context nodes into the context lines visible at
/// `scroll_position`. Cheap: a filter over an already-parsed node list, so it is
/// safe to call on every frame. Parsing happens once per file in
/// `AppState::get_context_nodes`.
pub fn context_lines_from_nodes(
    nodes: &[ContextNode],
    scroll_position: usize,
    config: &ContextConfig,
) -> Vec<ContextLine> {
    if !config.enabled || nodes.is_empty() || scroll_position == 0 {
        return Vec::new();
    }
    filter_to_lines(nodes, scroll_position, config.max_lines)
}

/// Uncached one-shot: parse `source` and compute the context lines for
/// `scroll_position`. The render path uses the cached
/// `collect_context_nodes` + `context_lines_from_nodes` pair instead.
#[cfg(test)]
pub fn compute_context_lines(
    source: &str,
    filename: &str,
    scroll_position: usize,
    config: &ContextConfig,
    tab_width: usize,
) -> Vec<ContextLine> {
    if !config.enabled || source.is_empty() || scroll_position == 0 {
        return Vec::new();
    }

    let nodes = collect_context_nodes(source, filename, tab_width);
    context_lines_from_nodes(&nodes, scroll_position, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_source() {
        let config = ContextConfig::default();
        let result = compute_context_lines("", "test.rs", 5, &config, 4);
        assert!(result.is_empty());
    }

    #[test]
    fn test_scroll_at_zero() {
        let config = ContextConfig::default();
        let source = "fn main() {\n    println!(\"hello\");\n}";
        let result = compute_context_lines(source, "test.rs", 0, &config, 4);
        assert!(result.is_empty());
    }

    #[test]
    fn test_rust_function_context() {
        let config = ContextConfig::default();
        let source = r#"fn main() {
    let x = 1;
    let y = 2;
    let z = 3;
    println!("{}", x + y + z);
}"#;
        let result = compute_context_lines(source, "test.rs", 3, &config, 4);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "fn main() {");
        assert_eq!(result[0].line_number, 1);
    }

    #[test]
    fn test_nested_rust_contexts() {
        let config = ContextConfig::default();
        let source = r#"impl Foo {
    fn bar() {
        if true {
            let x = 1;
        }
    }
}"#;
        let result = compute_context_lines(source, "test.rs", 3, &config, 4);
        // Should show impl, fn, and if
        assert!(result.len() >= 2);
        assert!(result[0].content.contains("impl Foo"));
    }

    #[test]
    fn test_unsupported_language() {
        let config = ContextConfig::default();
        let source = "some content\nmore content\neven more";
        let result = compute_context_lines(source, "test.xyz", 2, &config, 4);
        assert!(result.is_empty());
    }

    #[test]
    fn test_disabled_config() {
        let config = ContextConfig {
            enabled: false,
            max_lines: 5,
        };
        let source = "fn main() {\n    let x = 1;\n}";
        let result = compute_context_lines(source, "test.rs", 1, &config, 4);
        assert!(result.is_empty());
    }
}
