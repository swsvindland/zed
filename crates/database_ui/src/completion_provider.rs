//! SQL completion in consoles and bound `.sql` editors, from the connection's catalog metadata.

use std::{cell::RefCell, collections::HashSet, rc::Rc};

use anyhow::Result;
use database::completion::{self, Candidate};
use editor::{CompletionProvider, Editor};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, Task, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};

use util::ResultExt as _;

use crate::{DatabaseStore, console};

/// Editors that already have the SQL provider, so it's installed once.
#[derive(Default)]
struct SqlCompletionEditors(HashSet<EntityId>);

impl Global for SqlCompletionEditors {}

/// Gives `editor` SQL completion. Language servers and snippets keep working: their results
/// are offered after the catalog's.
pub fn install(editor: &Entity<Editor>, cx: &mut App) {
    if !cx
        .default_global::<SqlCompletionEditors>()
        .0
        .insert(editor.entity_id())
    {
        return;
    }
    // Deferred, since this can be reached while the editor itself is being updated.
    let editor = editor.downgrade();
    cx.defer(move |cx| {
        editor
            .update(cx, |editor, _| {
                let fallback = editor
                    .project()
                    .cloned()
                    .map(|project| Rc::new(project) as Rc<dyn CompletionProvider>);
                editor.set_completion_provider(Some(Rc::new(SqlCompletionProvider { fallback })));
            })
            .log_err();
    });
}

struct SqlCompletionProvider {
    fallback: Option<Rc<dyn CompletionProvider>>,
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        trigger: editor::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let fallback = self.fallback.as_ref().map(|fallback| {
            fallback.completions(buffer, buffer_position, trigger.clone(), window, cx)
        });
        let target = console::target_for_buffer(buffer, cx);
        let store = DatabaseStore::global(cx);
        let profile = target
            .as_ref()
            .and_then(|target| store.read(cx).profile(&target.connection_id).cloned());
        let (Some(target), Some(profile)) = (target, profile) else {
            return match fallback {
                Some(fallback) => fallback,
                None => Task::ready(Ok(Vec::new())),
            };
        };
        let dialect = profile.driver.dialect();
        let catalog = store.update(cx, |store, cx| {
            store.catalog(&target.connection_id, &target.database, cx)
        });
        let snapshot = buffer.read(cx).snapshot();
        let offset = buffer_position.to_offset(&snapshot);
        cx.spawn(async move |_, cx| {
            let catalog = catalog.await;
            let text = snapshot.text();
            let completions = cx
                .background_spawn(async move {
                    completion::complete(catalog.as_deref(), &text, offset, dialect)
                })
                .await;
            let replace_range = snapshot.anchor_before(completions.replace.start)
                ..snapshot.anchor_after(completions.replace.end);
            let mut responses = vec![CompletionResponse {
                completions: completions
                    .candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| {
                        to_completion(candidate, index, replace_range.clone())
                    })
                    .collect(),
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: false,
            }];
            if let Some(fallback) = fallback {
                match fallback.await {
                    Ok(more) => responses.extend(more),
                    Err(error) => log::debug!("other completions failed: {error:#}"),
                }
            }
            Ok(responses)
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        match &self.fallback {
            Some(fallback) => {
                fallback.resolve_completions(buffer, completion_indices, completions, cx)
            }
            None => Task::ready(Ok(false)),
        }
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<std::ops::Range<language::Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        match &self.fallback {
            Some(fallback) => fallback.apply_additional_edits_for_completion(
                buffer,
                completions,
                completion_index,
                push_to_history,
                all_commit_ranges,
                cx,
            ),
            None => Task::ready(Ok(None)),
        }
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let mut characters = text.chars();
        let Some(character) = characters.next() else {
            return false;
        };
        if characters.next().is_none()
            && (character == '.'
                || (trigger_in_words && (character.is_alphanumeric() || character == '_')))
        {
            return true;
        }
        self.fallback.as_ref().is_some_and(|fallback| {
            fallback.is_completion_trigger(buffer, position, text, trigger_in_words, cx)
        })
    }

    fn show_snippets(&self) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|fallback| fallback.show_snippets())
    }
}

fn to_completion(
    candidate: Candidate,
    index: usize,
    replace_range: std::ops::Range<language::Anchor>,
) -> Completion {
    Completion {
        replace_range,
        new_text: candidate.insert,
        label: CodeLabel::plain(candidate.label, None),
        documentation: candidate
            .detail
            .map(|detail| CompletionDocumentation::SingleLine(detail.into())),
        // The sort text ranks equally good matches by what fits the context best, then keeps
        // the catalog's order, so columns are listed as the table defines them.
        source: CompletionSource::Dap {
            sort_text: format!("{}{index:06}", candidate.priority),
        },
        icon_path: None,
        icon_color: None,
        match_start: None,
        snippet_deduplication_key: None,
        insert_text_mode: None,
        confirm: None,
        group: None,
    }
}
