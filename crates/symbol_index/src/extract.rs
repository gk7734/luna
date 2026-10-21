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

/// Names shorter than this are rarely worth completing and would crowd out real matches.
const MIN_SYMBOL_NAME_LEN: usize = 2;

pub fn extract_symbols(language: &Language, text: &str) -> Vec<IndexedSymbol> {
    let Some(grammar) = language.grammar() else {
        return Vec::new();
    };
    let Some(outline_config) = grammar.outline_config.as_ref() else {
        return Vec::new();
    };
    let Some(tree) = language::with_parser(|parser| {
        parser.set_language(&grammar.ts_language).ok()?;
        parser.parse(text, None)
    }) else {
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
        let symbols = extract_symbols(
            &language,
            "
            struct Point { x: i32 }
            enum Shape { Circle }
            const MAX_POINTS: usize = 3;
            fn distance(a: Point) -> f32 { 0.0 }
            mod geometry { pub fn area() {} }
            ",
        );
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
}
