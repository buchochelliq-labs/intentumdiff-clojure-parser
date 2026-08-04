//! Clojure parser plugin — full-parse mode.
//!
//! Handles `.clj`, `.cljs`, `.cljc`, `.edn` files.
//! Uses tree-sitter-clojure directly (no Python grammar package needed).
//!
//! Handles both specialised form nodes (newer grammars) and generic `list_lit` nodes.

use intentumdiff_plugin_sdk::ts_convert::{convert_ts_direct, TsDirectHooks};
use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct ClojureParser;

const TRIVIA: &[&str] = &["comment", "discard_expr", "dis_expr"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "source_file",
    "source",
    "program",
    // Specialised form nodes (newer grammars)
    "ns_form",
    "defn_form",
    "defn-_form",
    "def_form",
    "defonce_form",
    "defmacro_form",
    "defmulti_form",
    "defmethod_form",
    "defprotocol_form",
    "defrecord_form",
    "deftype_form",
    "definterface_form",
    "defstruct_form",
    "require_form",
    "use_form",
    "import_form",
    "let_form",
    "letfn_form",
    "if_form",
    "when_form",
    "when-not_form",
    "cond_form",
    "case_form",
    "condp_form",
    "do_form",
    "fn_form",
    "loop_form",
    "for_form",
    "doseq_form",
    "dotimes_form",
    "try_form",
    "catch_form",
    "finally_form",
    "throw_form",
    // Generic list form (minimal grammars)
    "list_lit",
    // Literals and data structures
    "vec_lit",
    "map_lit",
    "set_lit",
    "anon_fn_lit",
    "str_lit",
    "num_lit",
    "kwd_lit",
    "sym_lit",
    "bool_lit",
    "nil_lit",
    "regex_lit",
    "char_lit",
    // Metadata / reader macros
    "meta_lit",
    "deref_lit",
    "quote_lit",
    "syn_quote_lit",
    "unquote_lit",
    "unquote_splicing_lit",
    "var_lit",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

/// Clojure uses list_lit for all named forms; class/method classification is
/// handled by is_class_like_list_ts / is_method_like_list_ts instead.
fn is_class_like(_node_type: &str) -> bool {
    false
}

fn is_method_like(_node_type: &str) -> bool {
    false
}

/// Inspect the leading symbol of a list form to classify it.
/// Returns `Some("method")`, `Some("class")`, `Some("var")`, `Some("ns")` or `None`.
fn classify_list_form_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<&'static str> {
    let mut first_symbol: Option<String> = None;
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        if matches!(child.kind(), "sym_lit" | "identifier") {
            first_symbol = Some(child.utf8_text(source).unwrap_or("").to_string());
            break;
        }
    }
    let sym = first_symbol.as_deref()?;
    match sym {
        "defn" | "defn-" | "defmacro" | "defmulti" | "defmethod" | "defmemoize" => Some("method"),
        "defprotocol" | "defrecord" | "deftype" | "definterface" | "defstruct" => Some("class"),
        "ns" => Some("ns"),
        "def" | "defonce" | "defvar" => Some("var"),
        _ => None,
    }
}

fn is_class_like_list_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    node.kind() == "list_lit"
        && matches!(
            classify_list_form_ts(node, source),
            Some("class") | Some("ns")
        )
}

fn is_method_like_list_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    node.kind() == "list_lit" && matches!(classify_list_form_ts(node, source), Some("method"))
}

fn label_for_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let kind = node.kind();
    let txt = |n: tree_sitter::Node<'_>| n.utf8_text(source).unwrap_or("").to_string();
    if matches!(
        kind,
        "sym_lit"
            | "kwd_lit"
            | "num_lit"
            | "bool_lit"
            | "nil_lit"
            | "regex_lit"
            | "char_lit"
            | "identifier"
    ) {
        return txt(node);
    }
    if node.child_count() == 0 {
        return node.utf8_text(source).unwrap_or("").to_string();
    }

    let is_form = is_class_like(kind)
        || is_method_like(kind)
        || matches!(kind, "def_form" | "defonce_form" | "ns_form")
        || (kind == "list_lit" && classify_list_form_ts(node, source).is_some());

    if is_form {
        let mut saw_keyword = false;
        for i in 0..node.child_count() {
            let child = node.child(i).unwrap();
            let k = child.kind();
            if !saw_keyword && matches!(k, "sym_lit" | "kwd_lit" | "identifier") {
                saw_keyword = true;
                continue;
            }
            if saw_keyword && matches!(k, "sym_lit" | "identifier" | "kwd_lit") {
                return txt(child);
            }
        }
    }

    match kind {
        "vec_lit" | "map_lit" | "set_lit" => return kind.to_string(),
        "str_lit" => {
            let text = node.utf8_text(source).unwrap_or("");
            let trimmed = text.trim_matches('"');
            let truncated = if trimmed.len() > 40 {
                &trimmed[..40]
            } else {
                trimmed
            };
            return format!("\"{}\"", truncated);
        }
        _ => {}
    }

    for i in 0..node.child_count() {
        let c = node.child(i).unwrap();
        if c.kind() == "sym_lit" || c.kind() == "identifier" {
            return txt(c);
        }
    }
    kind.to_string()
}

fn convert_ts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    id_prefix: &str,
    parent_class: Option<&str>,
) -> Option<SemanticNode> {
    convert_ts_direct(
        node,
        source,
        id_prefix,
        parent_class,
        &TsDirectHooks {
            is_trivia: &|kind| TRIVIA.contains(&kind),
            class_label: &|n, s| (is_class_like(n.kind()) || is_class_like_list_ts(n, s))
                .then(|| label_for_ts(n, s)),
            keep_childless: &|n| is_semantic(n.kind()),
            unwrap_single: &|_, _| false,
            label: &|n, s| label_for_ts(n, s),
            is_method_like: &|n| is_method_like(n.kind()) || is_method_like_list_ts(n, source),
        },
    )
}

fn process_impl(source: &str) -> String {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_clojure::LANGUAGE.into();
    if parser.set_language(&lang).is_err() {
        return r#"{"error":"Failed to load Clojure grammar"}"#.to_string();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return r#"{"error":"Parse failed"}"#.to_string(),
    };
    let root = tree.root_node();
    match convert_ts(root, source.as_bytes(), "0", None) {
        Some(n) => serde_json::to_string(&n).unwrap_or_else(|e| format!(r#"{{"error":"{}"}}"#, e)),
        None => r#"{"error":"Empty semantic tree"}"#.to_string(),
    }
}
impl Guest for ClojureParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "clojure".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".clj")
            || lower.ends_with(".cljs")
            || lower.ends_with(".cljc")
            || lower.ends_with(".edn")
        {
            return "clojure".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["clojure".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "(defn greet [name]\n  (println (str \"Hello, \" name)))\n\n(defn add [a b]\n  (+ a b))\n".to_string(),
            new: "(defn greet\n  ([name] (greet name \"!\"))\n  ([name suffix]\n   (println (str \"Hello, \" name suffix))))\n\n(defn add [x y] (+ x y))\n\n(defn multiply [x y] (* x y))\n".to_string(),
        }
    }
}
export!(ClojureParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentumdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!ClojureParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = ClojureParser::grammar_id();
        let ids = ClojureParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = ClojureParser::detect_language("test.clj".to_string(), "".to_string());
        assert_eq!(r.as_str(), "clojure");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r = ClojureParser::detect_language(
            "test.xyz_notareal_ext_9z8y".to_string(),
            "".to_string(),
        );
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
