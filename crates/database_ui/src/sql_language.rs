//! Registers a built-in SQL language, since Zed ships no SQL grammar of its own.

use std::{borrow::Cow, sync::Arc};

use futures::FutureExt as _;
use language::{LanguageConfig, LanguageName, LanguageQueries, LanguageRegistry, LoadedLanguage};

const CONFIG: &str = r#"
name = "SQL"
grammar = "sql"
path_suffixes = ["sql"]
line_comments = ["-- "]
block_comment = { start = "/*", prefix = "", end = "*/", tab_size = 1 }
autoclose_before = ";:.,=}])"
brackets = [
    { start = "(", end = ")", close = true, newline = true },
    { start = "[", end = "]", close = true, newline = false, not_in = ["string", "comment"] },
    { start = "'", end = "'", close = true, newline = false, not_in = ["string", "comment"] },
    { start = "\"", end = "\"", close = true, newline = false, not_in = ["string", "comment"] },
]
"#;

pub(crate) fn register(languages: &Arc<LanguageRegistry>) {
    let name = LanguageName::new_static("SQL");
    if languages.language_names().contains(&name) {
        return;
    }
    let config = match toml::from_str::<LanguageConfig>(CONFIG) {
        Ok(config) => config,
        Err(error) => {
            log::error!("invalid built-in SQL language config: {error}");
            return;
        }
    };
    languages.register_native_grammars([("sql", tree_sitter_sequel::LANGUAGE)]);
    languages.register_language(
        name,
        config.grammar.clone(),
        config.matcher.clone(),
        false,
        None,
        Arc::new(move || {
            let config = config.clone();
            async move {
                Ok(LoadedLanguage {
                    config,
                    queries: LanguageQueries {
                        highlights: Some(Cow::Owned(highlights_query())),
                        ..LanguageQueries::default()
                    },
                    context_provider: None,
                    toolchain_provider: None,
                    manifest_name: None,
                })
            }
            .boxed()
        }),
    );
}

/// The grammar's query targets Neovim capture names and Lua patterns; map both to what Zed's
/// themes and regex engine understand.
fn highlights_query() -> String {
    let mut query = tree_sitter_sequel::HIGHLIGHTS_QUERY.replace("%d", "[0-9]");
    for (from, to) in [
        ("@function.call", "@function"),
        ("@conditional", "@keyword"),
        ("@storageclass", "@keyword"),
        ("@type.qualifier", "@keyword"),
        ("@keyword.operator", "@keyword"),
        ("@field", "@property"),
        ("@parameter", "@variable.special"),
        ("@float", "@number"),
        (" @spell", ""),
    ] {
        query = query.replace(from, to);
    }
    query
}

#[cfg(test)]
mod tests {
    #[test]
    fn highlights_query_compiles() {
        let language: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
        tree_sitter::Query::new(&language, &super::highlights_query()).unwrap();
    }
}
