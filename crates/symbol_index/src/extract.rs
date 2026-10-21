use language::Language;
use std::sync::Arc;
use tree_sitter::{QueryCursor, StreamingIterator as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Type,
    Constant,
    Variable,
    Module,
    Other,
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Type => "type",
            SymbolKind::Constant => "constant",
            SymbolKind::Variable => "variable",
            SymbolKind::Module => "module",
            SymbolKind::Other => "symbol",
        }
    }

    /// Outline queries don't capture a symbol kind, only `@context` keywords such as `fn` or
    /// `struct` that precede the name, so the kind is inferred from those.
    fn from_context_keywords<'a>(keywords: impl IntoIterator<Item = &'a str>) -> Self {
        let mut kind = SymbolKind::Other;
        for keyword in keywords {
            let keyword_kind = match keyword {
                "fn" | "func" | "function" | "def" | "fun" | "method" | "async" => {
                    SymbolKind::Function
                }
                "struct" | "enum" | "class" | "interface" | "trait" | "type" | "union"
                | "record" | "typedef" | "protocol" => SymbolKind::Type,
                "const" | "static" => SymbolKind::Constant,
                "let" | "var" | "val" => SymbolKind::Variable,
                "mod" | "module" | "namespace" | "package" => SymbolKind::Module,
                _ => continue,
            };
            if kind == SymbolKind::Other {
                kind = keyword_kind;
            }
        }
        kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSymbol {
    pub name: Arc<str>,
    pub kind: SymbolKind,
}

/// A TODO or FIXME comment, found the way JetBrains IDEs do by default: the keyword as a whole
/// word in any case, inside a comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem {
    pub row: u32,
    pub column: u32,
    pub text: Arc<str>,
}

const TODO_KEYWORDS: [&str; 2] = ["todo", "fixme"];

#[derive(Default)]
pub struct FileIndex {
    pub symbols: Vec<IndexedSymbol>,
    pub todos: Vec<TodoItem>,
}

/// Parses the file once for both its outline symbols and its TODO comments.
pub fn index_text(language: &Language, text: &str) -> FileIndex {
    let Some(grammar) = language.grammar() else {
        return FileIndex::default();
    };
    let Some(tree) = language::with_parser(|parser| {
        parser.set_language(&grammar.ts_language).ok()?;
        parser.parse(text, None)
    }) else {
        return FileIndex::default();
    };
    FileIndex {
        symbols: symbols_in_tree(language, &tree, text),
        todos: todos_in_tree(&tree, text),
    }
}

/// Names shorter than this are rarely worth completing and would crowd out real matches.
const MIN_SYMBOL_NAME_LEN: usize = 2;

fn symbols_in_tree(
    language: &Language,
    tree: &tree_sitter::Tree,
    text: &str,
) -> Vec<IndexedSymbol> {
    let Some(outline_config) = language
        .grammar()
        .and_then(|grammar| grammar.outline_config.as_ref())
    else {
        return Vec::new();
    };

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&outline_config.query, tree.root_node(), text.as_bytes());
    while let Some(query_match) = matches.next() {
        let mut name = None;
        let mut has_item = false;
        let mut context_keywords = Vec::new();
        for capture in query_match.captures {
            let Some(capture_text) = text.get(capture.node.byte_range()) else {
                continue;
            };
            if capture.index == outline_config.name_capture_ix {
                name = Some(capture_text);
            } else if capture.index == outline_config.item_capture_ix {
                has_item = true;
            } else if Some(capture.index) == outline_config.context_capture_ix {
                context_keywords.push(capture_text);
            }
        }
        let Some(name) = name.filter(|name| is_completable_name(name)) else {
            continue;
        };
        if !has_item
            || symbols
                .iter()
                .any(|symbol: &IndexedSymbol| &*symbol.name == name)
        {
            continue;
        }
        symbols.push(IndexedSymbol {
            name: name.into(),
            kind: SymbolKind::from_context_keywords(context_keywords),
        });
    }
    symbols
}

/// Grammars name their comment nodes `comment`, `line_comment`, `block_comment` and so on.
fn todos_in_tree(tree: &tree_sitter::Tree, text: &str) -> Vec<TodoItem> {
    let mut todos = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let is_comment = node.kind().contains("comment");
        if is_comment {
            let start = node.start_position();
            if let Some(comment_text) = text.get(node.byte_range()) {
                for (line_index, line) in comment_text.lines().enumerate() {
                    let line_start_column = if line_index == 0 { start.column } else { 0 };
                    if let Some((column, todo_text)) = find_todo(line) {
                        todos.push(TodoItem {
                            row: (start.row + line_index) as u32,
                            column: (line_start_column + column) as u32,
                            text: todo_text.into(),
                        });
                    }
                }
            }
        }
        if !is_comment && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return todos;
            }
        }
    }
}

/// Returns the byte column of the keyword and the text from the keyword to the end of the line.
fn find_todo(line: &str) -> Option<(usize, &str)> {
    let lowercase_line = line.to_ascii_lowercase();
    let is_word_character = |character: Option<char>| {
        character.is_some_and(|character| character.is_alphanumeric() || character == '_')
    };
    for keyword in TODO_KEYWORDS {
        for (column, _) in lowercase_line.match_indices(keyword) {
            let before = lowercase_line[..column].chars().next_back();
            let after = lowercase_line[column + keyword.len()..].chars().next();
            if is_word_character(before) || is_word_character(after) {
                continue;
            }
            let todo_text = line[column..].trim_end().trim_end_matches("*/").trim_end();
            return Some((column, todo_text));
        }
    }
    None
}

fn is_completable_name(name: &str) -> bool {
    name.chars().count() >= MIN_SYMBOL_NAME_LEN
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_' || character == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use language::rust_lang;

    #[test]
    fn test_extract_rust_symbols() {
        let language = rust_lang();
        let symbols = index_text(
            &language,
            "
            struct Point { x: i32 }
            enum Shape { Circle }
            const MAX_POINTS: usize = 3;
            fn distance(a: Point) -> f32 { 0.0 }
            mod geometry { pub fn area() {} }
            ",
        )
        .symbols;
        let kinds = symbols
            .iter()
            .map(|symbol| (&*symbol.name, symbol.kind))
            .collect::<Vec<_>>();
        for expected in [
            ("Point", SymbolKind::Type),
            ("Shape", SymbolKind::Type),
            ("MAX_POINTS", SymbolKind::Constant),
            ("distance", SymbolKind::Function),
            ("geometry", SymbolKind::Module),
            ("area", SymbolKind::Function),
        ] {
            assert!(
                kinds.contains(&expected),
                "missing {expected:?} in {kinds:?}"
            );
        }
    }

    #[test]
    fn test_extract_todo_comments() {
        let language = rust_lang();
        let text = "// TODO: handle errors\nfn todo_list() { todo!() }\n/* first line\n   fixme later */\nlet todos = 1; // not a todos match\n// Todo lowercase too\n";
        let todos = index_text(&language, text).todos;
        assert_eq!(
            todos,
            vec![
                TodoItem {
                    row: 0,
                    column: 3,
                    text: "TODO: handle errors".into(),
                },
                TodoItem {
                    row: 3,
                    column: 3,
                    text: "fixme later".into(),
                },
                TodoItem {
                    row: 5,
                    column: 3,
                    text: "Todo lowercase too".into(),
                },
            ]
        );
    }
}
