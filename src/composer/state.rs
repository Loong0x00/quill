use std::collections::HashSet;
use std::sync::{mpsc, Arc};
use std::time::Instant;

use crate::completion::{
    GenerationId, ProviderRegistry, ProviderResult, QueryCtx, Suggestion, WorkerPool,
};
use crate::composer::fuzzy::fuzzy_filter;
use crate::composer::prompt_track::{Osc133Event, Osc133Scanner, Segment};
use crate::composer::tokenizer::{tokenize, TokenKind, Tokenized};

pub struct ComposerState {
    active: bool,
    buffer: String,
    cursor: usize,
    scanner: Osc133Scanner,
    in_prompt: bool,
    last_keystroke_at: Instant,
    pending_gen: GenerationId,
    ime_preedit_active: bool,
    terminal_in_alt_screen: bool,
    registry: Arc<ProviderRegistry>,
    worker_pool: Arc<WorkerPool>,
    result_tx: mpsc::Sender<ProviderResult>,
    result_rx: mpsc::Receiver<ProviderResult>,
    raw_candidates: Vec<Suggestion>,
    candidates: Vec<Suggestion>,
    selected: Option<usize>,
    pending_provider_results: usize,
    debounce_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerInput {
    Char(char),
    Backspace,
    Delete,
    LeftArrow,
    RightArrow,
    Home,
    End,
    Tab,
    BackTab,
    Enter,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerOutcome {
    Consumed,
    Passthrough,
    Submit(String),
}

impl ComposerState {
    pub fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            active: false,
            buffer: String::new(),
            cursor: 0,
            scanner: Osc133Scanner::new(),
            in_prompt: false,
            last_keystroke_at: Instant::now(),
            pending_gen: GenerationId(0),
            ime_preedit_active: false,
            terminal_in_alt_screen: false,
            registry: Arc::new(ProviderRegistry::new_default()),
            worker_pool: Arc::new(WorkerPool::new(4)),
            result_tx,
            result_rx,
            raw_candidates: Vec::new(),
            candidates: Vec::new(),
            selected: None,
            pending_provider_results: 0,
            debounce_requested: false,
        }
    }

    pub fn feed_pty_output<'a>(&'a mut self, bytes: &'a [u8]) -> Vec<Segment<'a>> {
        let segments = self.scanner.feed(bytes);
        for segment in &segments {
            match segment {
                Segment::Marker(Osc133Event::PromptStart) => {
                    self.active = true;
                    self.in_prompt = true;
                    self.buffer.clear();
                    self.cursor = 0;
                    self.raw_candidates.clear();
                    self.candidates.clear();
                    self.selected = None;
                }
                Segment::Marker(Osc133Event::InputStart) => {}
                Segment::Marker(Osc133Event::CommandStart | Osc133Event::CommandDone(_)) => {
                    self.active = false;
                    self.in_prompt = false;
                    self.raw_candidates.clear();
                    self.candidates.clear();
                    self.selected = None;
                }
                Segment::Bytes(_) => {}
            }
        }
        segments
    }

    pub fn set_ime_preedit_active(&mut self, active: bool) {
        self.ime_preedit_active = active;
    }

    pub fn set_alt_screen(&mut self, in_alt: bool) {
        self.terminal_in_alt_screen = in_alt;
    }

    pub fn is_active(&self) -> bool {
        self.active && self.in_prompt && !self.ime_preedit_active && !self.terminal_in_alt_screen
    }

    pub fn handle_input(&mut self, input: ComposerInput) -> ComposerOutcome {
        if !self.is_active() {
            return ComposerOutcome::Passthrough;
        }

        match input {
            ComposerInput::Char(c) => {
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.touch();
                ComposerOutcome::Consumed
            }
            ComposerInput::Backspace => {
                if let Some(prev) = prev_char_boundary(&self.buffer, self.cursor) {
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.touch();
                }
                ComposerOutcome::Consumed
            }
            ComposerInput::Delete => {
                if self.cursor < self.buffer.len() {
                    let next = next_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(self.cursor..next);
                    self.touch();
                }
                ComposerOutcome::Consumed
            }
            ComposerInput::LeftArrow => {
                if let Some(prev) = prev_char_boundary(&self.buffer, self.cursor) {
                    self.cursor = prev;
                    self.touch();
                }
                ComposerOutcome::Consumed
            }
            ComposerInput::RightArrow => {
                if self.popup_visible() {
                    self.accept_selected();
                    self.touch();
                } else if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                    self.touch();
                }
                ComposerOutcome::Consumed
            }
            ComposerInput::Home => {
                if self.cursor != 0 {
                    self.cursor = 0;
                    self.touch();
                }
                ComposerOutcome::Consumed
            }
            ComposerInput::End => {
                if self.cursor != self.buffer.len() {
                    self.cursor = self.buffer.len();
                    self.touch();
                }
                ComposerOutcome::Consumed
            }
            ComposerInput::Tab => {
                self.select_next();
                ComposerOutcome::Consumed
            }
            ComposerInput::BackTab => {
                self.select_prev();
                ComposerOutcome::Consumed
            }
            ComposerInput::Enter => {
                let submitted = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                self.clear_candidates();
                self.debounce_requested = false;
                ComposerOutcome::Submit(submitted)
            }
            ComposerInput::Escape => {
                self.active = false;
                self.buffer.clear();
                self.cursor = 0;
                self.clear_candidates();
                self.debounce_requested = false;
                ComposerOutcome::Consumed
            }
        }
    }

    pub fn current_tokenization(&self) -> Tokenized {
        tokenize(&self.buffer, self.cursor)
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn candidates(&self) -> &[Suggestion] {
        &self.candidates
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn popup_visible(&self) -> bool {
        self.is_active() && !self.candidates.is_empty()
    }

    pub fn take_debounce_request(&mut self) -> bool {
        let requested = self.debounce_requested;
        self.debounce_requested = false;
        requested
    }

    pub fn has_pending_results(&self) -> bool {
        self.pending_provider_results > 0
    }

    pub fn trigger_query(&mut self) {
        if !self.is_active() {
            self.clear_candidates();
            return;
        }

        let Some(ctx) = self.query_ctx() else {
            self.clear_candidates();
            return;
        };

        self.raw_candidates.clear();
        self.candidates.clear();
        self.selected = None;
        self.pending_provider_results = self.registry.provider_count();
        self.registry.query_all(
            ctx,
            self.pending_gen,
            &self.worker_pool,
            self.result_tx.clone(),
        );
    }

    pub fn poll_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok((gen_id, suggestions, provider)) = self.result_rx.try_recv() {
            if gen_id != self.pending_gen {
                continue;
            }
            self.pending_provider_results = self.pending_provider_results.saturating_sub(1);
            if suggestions.is_empty() {
                continue;
            }

            let before = self.raw_candidates.len();
            self.merge_candidates(suggestions);
            if self.raw_candidates.len() != before {
                let query = self.current_query_text();
                self.candidates = fuzzy_filter(self.raw_candidates.clone(), &query);
                self.selected = (!self.candidates.is_empty()).then_some(
                    self.selected
                        .unwrap_or(0)
                        .min(self.candidates.len().saturating_sub(1)),
                );
                changed = true;
                tracing::trace!(
                    target: "quill::completion",
                    provider,
                    gen = gen_id.0,
                    candidates = self.candidates.len(),
                    "completion results merged"
                );
            }
        }
        changed
    }

    fn touch(&mut self) {
        let old_gen = self.pending_gen;
        self.pending_gen = GenerationId(self.pending_gen.0.wrapping_add(1));
        self.worker_pool.cancel(old_gen);
        self.pending_provider_results = 0;
        self.clear_candidates();
        self.debounce_requested = true;
        self.last_keystroke_at = Instant::now();
    }

    fn clear_candidates(&mut self) {
        self.raw_candidates.clear();
        self.candidates.clear();
        self.selected = None;
    }

    fn select_next(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0);
        self.selected = Some((current + 1) % self.candidates.len());
    }

    fn select_prev(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0);
        self.selected = Some((current + self.candidates.len() - 1) % self.candidates.len());
    }

    fn accept_selected(&mut self) {
        let Some(idx) = self.selected else { return };
        let Some(suggestion) = self.candidates.get(idx) else {
            return;
        };
        let replacement = suggestion.text.clone();
        let tokenized = self.current_tokenization();
        if let Some(token_idx) = tokenized.current_token_idx {
            let token = &tokenized.tokens[token_idx];
            self.buffer
                .replace_range(token.start..token.end, &replacement);
            self.cursor = token.start + replacement.len();
        } else {
            self.buffer.insert_str(self.cursor, &replacement);
            self.cursor += replacement.len();
        }
    }

    fn merge_candidates(&mut self, suggestions: Vec<Suggestion>) {
        let mut seen = self
            .raw_candidates
            .iter()
            .map(|suggestion| suggestion.text.clone())
            .collect::<HashSet<_>>();
        for suggestion in suggestions {
            if seen.insert(suggestion.text.clone()) {
                self.raw_candidates.push(suggestion);
            }
        }
    }

    fn current_query_text(&self) -> String {
        let tokenized = self.current_tokenization();
        tokenized
            .current_token_idx
            .and_then(|idx| tokenized.tokens.get(idx))
            .map(|token| token.text.clone())
            .unwrap_or_default()
    }

    fn query_ctx(&self) -> Option<QueryCtx> {
        let tokenized = self.current_tokenization();
        let segment_start = tokenized
            .tokens
            .iter()
            .enumerate()
            .take_while(|(_, token)| token.start <= self.cursor)
            .filter(|(_, token)| matches!(&token.kind, TokenKind::Pipe | TokenKind::Sequence))
            .map(|(idx, _)| idx + 1)
            .last()
            .unwrap_or(0);

        let current_idx = tokenized.current_token_idx;
        let words = tokenized
            .tokens
            .iter()
            .enumerate()
            .skip(segment_start)
            .filter(|(_, token)| matches!(&token.kind, TokenKind::Word | TokenKind::Unterminated))
            .collect::<Vec<_>>();
        let command = words
            .first()
            .map(|(_, token)| token.text.clone())
            .unwrap_or_default();
        if command.is_empty() {
            return None;
        }

        let current_token = current_idx
            .and_then(|idx| tokenized.tokens.get(idx))
            .filter(|token| matches!(&token.kind, TokenKind::Word | TokenKind::Unterminated))
            .map(|token| token.text.clone())
            .unwrap_or_default();
        let previous_tokens = words
            .into_iter()
            .filter(|(idx, _)| Some(*idx) != current_idx)
            .filter(|(_, token)| token.start < self.cursor || current_idx.is_none())
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();

        Some(QueryCtx {
            command,
            current_token,
            previous_tokens,
            working_dir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        })
    }
}

fn prev_char_boundary(buffer: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    buffer[..cursor].char_indices().last().map(|(idx, _)| idx)
}

fn next_char_boundary(buffer: &str, cursor: usize) -> usize {
    buffer[cursor..]
        .chars()
        .next()
        .map_or(cursor, |ch| cursor + ch.len_utf8())
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;
    use ComposerInput::*;
    use ComposerOutcome::*;

    macro_rules! state_test {
        ($name:ident, $body:block) => {
            #[test]
            fn $name() $body
        };
    }

    fn activate(state: &mut ComposerState) {
        let segments = state.feed_pty_output(b"\x1b]133;A\x07");
        drop(segments);
    }
    fn active_state() -> ComposerState {
        let mut state = ComposerState::new();
        activate(&mut state);
        state
    }
    fn type_chars(state: &mut ComposerState, text: &str) {
        for ch in text.chars() {
            assert_eq!(state.handle_input(Char(ch)), Consumed);
        }
    }
    state_test!(test_initial_state, { let state = ComposerState::new(); assert!(!state.is_active()); assert_eq!(state.buffer(), ""); assert_eq!(state.cursor(), 0); });
    state_test!(test_prompt_start_activates, { let mut state = ComposerState::new(); state.buffer = "stale".into(); state.cursor = 5; let segments = state.feed_pty_output(b"\x1b]133;A\x07"); assert_eq!(segments, vec![Segment::Marker(Osc133Event::PromptStart)]); drop(segments); assert!(state.is_active()); assert_eq!(state.buffer(), ""); assert_eq!(state.cursor(), 0); });
    state_test!(test_command_start_deactivates, { let mut state = active_state(); state.feed_pty_output(b"\x1b]133;C\x07"); assert!(!state.is_active()); });
    state_test!(test_gate_blocks_when_ime, { let mut state = active_state(); state.set_ime_preedit_active(true); assert_eq!(state.handle_input(Char('x')), Passthrough); assert_eq!(state.buffer(), ""); });
    state_test!(test_gate_blocks_when_alt_screen, { let mut state = active_state(); state.set_alt_screen(true); assert_eq!(state.handle_input(Char('x')), Passthrough); assert_eq!(state.buffer(), ""); });
    state_test!(test_char_insert_at_cursor, { let mut state = active_state(); type_chars(&mut state, "ab"); state.handle_input(LeftArrow); state.handle_input(Char('x')); assert_eq!(state.buffer(), "axb"); assert_eq!(state.cursor(), 2); });
    state_test!(test_backspace_deletes_prev_char, { let mut state = active_state(); type_chars(&mut state, "ab"); state.handle_input(Backspace); assert_eq!(state.buffer(), "a"); assert_eq!(state.cursor(), 1); });
    state_test!(test_backspace_handles_multibyte, { let mut state = active_state(); type_chars(&mut state, "a你"); state.handle_input(Backspace); assert_eq!(state.buffer(), "a"); assert_eq!(state.cursor(), 1); });
    state_test!(test_arrow_moves_cursor, { let mut state = active_state(); type_chars(&mut state, "a你b"); state.handle_input(LeftArrow); assert_eq!(state.cursor(), 4); state.handle_input(LeftArrow); assert_eq!(state.cursor(), 1); state.handle_input(RightArrow); assert_eq!(state.cursor(), 4); state.handle_input(Home); assert_eq!(state.cursor(), 0); state.handle_input(End); assert_eq!(state.cursor(), 5); });
    state_test!(test_enter_returns_submit_and_clears, { let mut state = active_state(); type_chars(&mut state, "ls"); assert_eq!(state.handle_input(Enter), Submit("ls".into())); assert_eq!(state.buffer(), ""); assert_eq!(state.cursor(), 0); });
    state_test!(test_escape_clears_returns_consumed, { let mut state = active_state(); type_chars(&mut state, "x"); assert_eq!(state.handle_input(Escape), Consumed); assert_eq!(state.buffer(), ""); assert!(!state.is_active()); });
    state_test!(test_pending_gen_increments_on_keystroke, { let mut state = active_state(); assert_eq!(state.pending_gen, GenerationId(0)); state.handle_input(Char('a')); state.handle_input(LeftArrow); assert_eq!(state.pending_gen, GenerationId(2)); });
    state_test!(test_pty_segments_passthrough_non_133, { let mut state = ComposerState::new(); assert_eq!(state.feed_pty_output(b"abc\x1b]0;title\x07def"), vec![Segment::Bytes(b"abc\x1b]0;title\x07def")]); });
    state_test!(test_tab_cycles_candidates_without_query, { let mut state = active_state(); state.candidates = vec![crate::completion::Suggestion { text: "a".into(), display: "a".into(), description: String::new(), group: crate::completion::SuggestionGroup::Dynamic }, crate::completion::Suggestion { text: "b".into(), display: "b".into(), description: String::new(), group: crate::completion::SuggestionGroup::Dynamic }]; state.selected = Some(0); assert_eq!(state.handle_input(Tab), Consumed); assert_eq!(state.selected(), Some(1)); assert!(!state.take_debounce_request()); assert_eq!(state.handle_input(BackTab), Consumed); assert_eq!(state.selected(), Some(0)); });
    state_test!(test_right_accepts_selected_candidate, { let mut state = active_state(); type_chars(&mut state, "git ch"); state.candidates = vec![crate::completion::Suggestion { text: "checkout".into(), display: "checkout".into(), description: String::new(), group: crate::completion::SuggestionGroup::Subcommand }]; state.selected = Some(0); assert_eq!(state.handle_input(RightArrow), Consumed); assert_eq!(state.buffer(), "git checkout"); assert_eq!(state.cursor(), "git checkout".len()); assert!(state.take_debounce_request()); });
}
