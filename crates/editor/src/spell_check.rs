use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc, time::Duration};

use anyhow::Result;
use collections::HashMap;
use gpui::{App, Entity, HighlightStyle, Task, UnderlineStyle, WeakEntity, Window, px};
use language::{Buffer, BufferId, BufferSnapshot, LanguageAwareStyling};
use lsp::LanguageServerId;
use project::{CodeAction, LspAction, ProjectTransaction};
use settings::Settings as _;
use text::{Anchor as TextAnchor, ToOffset as _};
use theme::ActiveTheme as _;
use ui::Context;

use crate::{CodeActionProvider, Editor, EditorSettings, display_map::HighlightKey};

const SPELL_CHECK_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_SUGGESTIONS: usize = 5;

/// Shared with the code action provider, which runs while the editor is being updated and so
/// can't read it.
type Misspellings = Rc<RefCell<HashMap<BufferId, Vec<Range<TextAnchor>>>>>;

#[derive(Default)]
pub(super) struct SpellCheckState {
    misspellings: Misspellings,
    check_task: Option<Task<()>>,
}

impl SpellCheckState {
    pub(super) fn code_action_provider(
        &self,
        editor: WeakEntity<Editor>,
    ) -> Rc<dyn CodeActionProvider> {
        Rc::new(SpellCheckActions {
            misspellings: self.misspellings.clone(),
            editor,
        })
    }
}

impl Editor {
    pub(super) fn schedule_spell_check(&mut self, cx: &mut Context<Self>) {
        if !self.mode().is_full()
            || !EditorSettings::get_global(cx).spell_check
            || !cx.has_spell_checker()
        {
            if !self.spell_check.misspellings.borrow().is_empty() {
                self.spell_check.misspellings.borrow_mut().clear();
                self.clear_highlights(HighlightKey::SpellCheck, cx);
            }
            return;
        }
        self.spell_check.check_task = Some(cx.spawn(async move |editor, cx| {
            cx.background_executor().timer(SPELL_CHECK_DEBOUNCE).await;
            editor
                .update(cx, |editor, cx| editor.check_visible_spelling(cx))
                .ok();
        }));
    }

    fn check_visible_spelling(&mut self, cx: &mut Context<Self>) {
        let syntax_theme = cx.theme().syntax().clone();
        let mut misspellings: HashMap<BufferId, Vec<Range<TextAnchor>>> = HashMap::default();
        for (buffer_snapshot, visible_range, _) in self.visible_buffer_ranges(cx) {
            let buffer_misspellings = misspellings.entry(buffer_snapshot.remote_id()).or_default();
            for (segment_start, segment_text) in prose_segments(
                &buffer_snapshot,
                visible_range.start.0..visible_range.end.0,
                &syntax_theme,
            ) {
                for range in cx.misspelled_ranges(&segment_text) {
                    if !is_checkable_word(&segment_text, &range) {
                        continue;
                    }
                    buffer_misspellings.push(
                        buffer_snapshot.anchor_after(segment_start + range.start)
                            ..buffer_snapshot.anchor_before(segment_start + range.end),
                    );
                }
            }
        }

        let multibuffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let highlight_ranges = misspellings
            .values()
            .flatten()
            .filter_map(|range| multibuffer_snapshot.anchor_range_in_buffer(range.clone()))
            .collect::<Vec<_>>();
        let had_misspellings = self
            .spell_check
            .misspellings
            .borrow()
            .values()
            .any(|ranges| !ranges.is_empty());
        *self.spell_check.misspellings.borrow_mut() = misspellings;
        // Highlighting redraws the editor, so leave it alone while there's nothing to show.
        if highlight_ranges.is_empty() {
            if had_misspellings {
                self.clear_highlights(HighlightKey::SpellCheck, cx);
            }
            return;
        }
        let underline_color = cx.theme().status().info;
        self.highlight_text(
            HighlightKey::SpellCheck,
            highlight_ranges,
            HighlightStyle {
                underline: Some(UnderlineStyle {
                    color: Some(underline_color),
                    thickness: px(1.),
                    wavy: true,
                }),
                ..HighlightStyle::default()
            },
            cx,
        );
    }
}

fn misspelling_at(
    misspellings: &Misspellings,
    buffer: &BufferSnapshot,
    range: &Range<TextAnchor>,
) -> Option<Range<TextAnchor>> {
    let start = range.start.to_offset(buffer);
    let end = range.end.to_offset(buffer);
    misspellings
        .borrow()
        .get(&buffer.remote_id())?
        .iter()
        .find(|misspelling| {
            misspelling.start.to_offset(buffer) <= start && end <= misspelling.end.to_offset(buffer)
        })
        .cloned()
}

/// Comments and strings, where prose lives, as (buffer offset, text) pairs.
fn prose_segments(
    buffer: &BufferSnapshot,
    range: Range<usize>,
    syntax_theme: &theme::SyntaxTheme,
) -> Vec<(usize, String)> {
    let mut segments: Vec<(usize, String)> = Vec::new();
    let mut offset = range.start;
    let mut previous_was_prose = false;
    let styling = LanguageAwareStyling {
        tree_sitter: true,
        diagnostics: false,
    };
    for chunk in buffer.chunks(range, styling) {
        let is_prose = chunk
            .syntax_highlight_id
            .and_then(|highlight_id| syntax_theme.get_capture_name(highlight_id))
            .is_some_and(|name| {
                name.starts_with("comment")
                    || name == "string"
                    || name == "string.doc"
                    || name == "string.special.symbol"
            });
        if is_prose {
            match segments.last_mut() {
                Some((_, text)) if previous_was_prose => text.push_str(chunk.text),
                _ => segments.push((offset, chunk.text.to_string())),
            }
        }
        previous_was_prose = is_prose;
        offset += chunk.text.len();
    }
    segments
}

/// Skips words that are code rather than prose: identifiers, abbreviations, paths and words in
/// other scripts, which the spell checker can't judge in a comment.
fn is_checkable_word(text: &str, range: &Range<usize>) -> bool {
    let Some(word) = text.get(range.clone()) else {
        return false;
    };
    let mut characters = word.chars();
    let Some(first_character) = characters.next() else {
        return false;
    };
    let is_plain_word = word.len() >= 3
        && word
            .chars()
            .all(|character| character.is_ascii_alphabetic() || character == '\'')
        && characters.all(|character| !character.is_ascii_uppercase())
        && first_character.is_ascii_alphabetic();
    let is_code_neighbor = |character: Option<char>| {
        matches!(
            character,
            Some('_' | '/' | '.' | '\\' | '@' | '#' | '`' | ':' | '-' | '$')
        )
    };
    let before = text[..range.start].chars().next_back();
    let after = text[range.end..].chars().next();
    is_plain_word && !is_code_neighbor(before) && !is_code_neighbor(after)
}

/// Offers the spell checker's suggestions and "Learn Spelling" for the misspelling under the
/// cursor.
struct SpellCheckActions {
    misspellings: Misspellings,
    editor: WeakEntity<Editor>,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum SpellingFix {
    Replace(String),
    Learn(String),
}

impl CodeActionProvider for SpellCheckActions {
    fn id(&self) -> Arc<str> {
        "spell_check".into()
    }

    fn code_actions(
        &self,
        buffer: &Entity<Buffer>,
        range: Range<TextAnchor>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        let buffer = buffer.read(cx).snapshot();
        let Some(misspelling) = misspelling_at(&self.misspellings, &buffer, &range) else {
            return Task::ready(Ok(Vec::new()));
        };
        let word = buffer
            .text_for_range(misspelling.clone())
            .collect::<String>();
        let action = |title: String, fix: SpellingFix| CodeAction {
            server_id: LanguageServerId(usize::MAX),
            range: misspelling.clone(),
            lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                title,
                kind: Some(lsp::CodeActionKind::QUICKFIX),
                data: serde_json::to_value(fix).ok(),
                ..lsp::CodeAction::default()
            })),
            resolved: true,
        };
        let mut actions = cx
            .spelling_suggestions(&word)
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .map(|suggestion| {
                action(
                    format!("Change to \u{201c}{suggestion}\u{201d}"),
                    SpellingFix::Replace(suggestion),
                )
            })
            .collect::<Vec<_>>();
        actions.push(action(
            format!("Learn Spelling of \u{201c}{word}\u{201d}"),
            SpellingFix::Learn(word),
        ));
        Task::ready(Ok(actions))
    }

    fn apply_code_action(
        &self,
        buffer: Entity<Buffer>,
        action: CodeAction,
        push_to_history: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<ProjectTransaction>> {
        let LspAction::Action(lsp_action) = &action.lsp_action else {
            return Task::ready(Ok(ProjectTransaction::default()));
        };
        let Some(fix) = lsp_action
            .data
            .clone()
            .and_then(|data| serde_json::from_value::<SpellingFix>(data).ok())
        else {
            return Task::ready(Ok(ProjectTransaction::default()));
        };
        let mut project_transaction = ProjectTransaction::default();
        match fix {
            SpellingFix::Replace(replacement) => {
                let transaction = buffer.update(cx, |buffer, cx| {
                    buffer.finalize_last_transaction();
                    buffer.start_transaction();
                    buffer.edit([(action.range.clone(), replacement)], None, cx);
                    buffer.end_transaction(cx);
                    let transaction = buffer.finalize_last_transaction().cloned();
                    if !push_to_history && let Some(transaction) = &transaction {
                        buffer.forget_transaction(transaction.id);
                    }
                    transaction
                });
                if let Some(transaction) = transaction {
                    project_transaction.0.insert(buffer, transaction);
                }
            }
            SpellingFix::Learn(word) => {
                cx.learn_spelling(&word);
                let editor = self.editor.clone();
                // Applying runs while the editor may be updating, so re-check once it's done.
                cx.defer(move |cx| {
                    editor
                        .update(cx, |editor, cx| editor.schedule_spell_check(cx))
                        .ok();
                });
            }
        }
        Task::ready(Ok(project_transaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Hsla, TestAppContext};
    use language::rust_lang;
    use theme::SyntaxTheme;

    #[gpui::test]
    async fn test_prose_segments_are_comments_and_strings(cx: &mut TestAppContext) {
        let syntax_theme = SyntaxTheme::new_test([
            ("comment", Hsla::default()),
            ("string", Hsla::default()),
            ("keyword", Hsla::default()),
        ]);
        let language = rust_lang();
        language.set_theme(&syntax_theme);
        let text = "// Helo wrold\nfn main() { let greeting = \"teh text\"; }\n";
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.run_until_parked();

        let segments = buffer.read_with(cx, |buffer, _| {
            prose_segments(&buffer.snapshot(), 0..text.len(), &syntax_theme)
        });
        let segment_texts = segments
            .iter()
            .map(|(offset, segment)| (*offset, segment.trim_end().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            segment_texts,
            vec![
                (0, "// Helo wrold".to_string()),
                (text.find("\"teh").unwrap(), "\"teh text\"".to_string()),
            ]
        );
    }

    #[gpui::test]
    async fn test_misspelled_comment_word_is_fixed_by_code_action(cx: &mut TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        cx.add_misspelled_word("wrold", &["world", "would"]);
        let mut cx = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        let language = rust_lang();
        cx.update(|_, cx| language.set_theme(cx.theme().syntax()));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(language), cx));
        cx.set_state("// hello wrold\nfn wroldˇ() {}\n");
        cx.run_until_parked();
        cx.executor().advance_clock(SPELL_CHECK_DEBOUNCE * 2);
        cx.run_until_parked();

        let (provider, buffer, misspellings) = cx.update_editor(|editor, _, cx| {
            let buffer = editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("test editor has one buffer");
            let snapshot = buffer.read(cx).snapshot();
            let misspellings = editor
                .spell_check
                .misspellings
                .borrow()
                .values()
                .flatten()
                .map(|range| snapshot.text_for_range(range.clone()).collect::<String>())
                .collect::<Vec<_>>();
            (
                editor.spell_check.code_action_provider(cx.weak_entity()),
                buffer,
                misspellings,
            )
        });
        // The function name is code, not prose.
        assert_eq!(misspellings, vec!["wrold".to_string()]);

        let word_start = cx.update(|_, cx| buffer.read(cx).anchor_after(9));
        let actions = cx
            .update(|window, cx| provider.code_actions(&buffer, word_start..word_start, window, cx))
            .await
            .expect("spell check actions don't fail");
        let titles = actions
            .iter()
            .map(|action| action.lsp_action.title().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            [
                "Change to \u{201c}world\u{201d}",
                "Change to \u{201c}would\u{201d}",
                "Learn Spelling of \u{201c}wrold\u{201d}",
            ]
        );

        cx.update(|window, cx| {
            provider.apply_code_action(buffer.clone(), actions[0].clone(), true, window, cx)
        })
        .await
        .expect("applying a spelling fix doesn't fail");
        cx.assert_editor_state("// hello world\nfn wroldˇ() {}\n");
    }

    #[test]
    fn test_code_like_words_are_skipped() {
        let checkable = |text: &str, word: &str| {
            let start = text.find(word).unwrap();
            is_checkable_word(text, &(start..start + word.len()))
        };
        assert!(checkable("// the recieve path", "recieve"));
        assert!(!checkable("// call foo_bar here", "bar"));
        assert!(!checkable("// see src/lsp.rs", "lsp"));
        assert!(!checkable("// uses parseJson", "parseJson"));
        assert!(!checkable("// the URL is fine", "URL"));
        assert!(!checkable("// abc 한국어", "한국어"));
        assert!(!checkable("// `cfg` value", "cfg"));
    }
}
