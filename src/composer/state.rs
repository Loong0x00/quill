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
    UpArrow,
    DownArrow,
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
    /// 接受候选时返回需要写到 PTY 的同步字节 (\x7f×旧 token 长度 + 替换字符串),
    /// 让 zsh prompt 显示也跟 buffer 同步.
    WritePty(Vec<u8>),
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
        let popup_visible = self.popup_visible();
        let buffer_empty = self.buffer.is_empty();

        match input {
            // 字符 / 删字符: 双写模型 — 走 PTY (zsh 回显) + 同时 buffer.insert.
            // 这里只更新 buffer (PTY 由 window.rs 那层负责), 永远 Consumed.
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
            // 移光标永远透传 — composer.cursor 不跟踪光标移动 (PTY 用户视觉
            // 为准, popup query 始终从 buffer 末尾倒推 token).
            ComposerInput::LeftArrow
            | ComposerInput::RightArrow
            | ComposerInput::Home
            | ComposerInput::End => ComposerOutcome::Passthrough,
            // Up/Down/Tab/BackTab: popup 可见时, 第一次按 → 直接 sync 当前 selected
            // (即第一项); 后续按 → select_next/prev + sync (cycle). 判定靠 current_token
            // 是否已等于 selected 文本 — 等于 = 已 sync = 这次该 cycle.
            ComposerInput::UpArrow => {
                if buffer_empty {
                    ComposerOutcome::Passthrough
                } else if popup_visible {
                    if self.current_token_matches_selected() {
                        self.select_prev();
                    }
                    ComposerOutcome::WritePty(self.sync_selected_to_buffer())
                } else {
                    ComposerOutcome::Consumed
                }
            }
            ComposerInput::DownArrow => {
                if buffer_empty {
                    ComposerOutcome::Passthrough
                } else if popup_visible {
                    if self.current_token_matches_selected() {
                        self.select_next();
                    }
                    ComposerOutcome::WritePty(self.sync_selected_to_buffer())
                } else {
                    ComposerOutcome::Consumed
                }
            }
            ComposerInput::Tab => {
                if popup_visible {
                    if self.current_token_matches_selected() {
                        self.select_next();
                    }
                    ComposerOutcome::WritePty(self.sync_selected_to_buffer())
                } else {
                    ComposerOutcome::Passthrough
                }
            }
            ComposerInput::BackTab => {
                if popup_visible {
                    if self.current_token_matches_selected() {
                        self.select_prev();
                    }
                    ComposerOutcome::WritePty(self.sync_selected_to_buffer())
                } else {
                    ComposerOutcome::Passthrough
                }
            }
            // Enter: popup 可见且有 selected → 接受候选写 PTY 同步 (不执行命令);
            // 否则透传让 zsh 执行.
            ComposerInput::Enter => {
                if popup_visible && self.selected.is_some() {
                    let bytes = self.accept_selected_with_diff();
                    ComposerOutcome::WritePty(bytes)
                } else {
                    ComposerOutcome::Passthrough
                }
            }
            // Esc: popup 可见 → 关 popup (clear candidates, buffer 不动); 否则透传.
            ComposerInput::Escape => {
                if popup_visible {
                    self.clear_candidates();
                    ComposerOutcome::Consumed
                } else {
                    ComposerOutcome::Passthrough
                }
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
        // 不 auto-sync — popup 出来只 highlight 不动 buffer. 用户按 Tab 才接受
        // (handle_input(Tab) 内 current_token_matches_selected 检查决定是接受
        // 第一项还是 cycle).
        changed
    }

    /// 当前 token 跟 selected 候选文本是否相同. 用来判断 Tab/Down 该 "接受当前
    /// 第一项" 还是 "cycle 到下一项": 不同 → 还没 sync, 直接 sync; 相同 → 已 sync,
    /// select_next 后再 sync.
    fn current_token_matches_selected(&self) -> bool {
        let tokenized = self.current_tokenization();
        let token_text = tokenized
            .current_token_idx
            .and_then(|i| tokenized.tokens.get(i))
            .map(|t| &self.buffer[t.start..t.end]);
        let sel_text = self
            .selected
            .and_then(|i| self.candidates.get(i).map(|s| s.text.as_str()));
        matches!((token_text, sel_text), (Some(a), Some(b)) if a == b)
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

    /// 接受当前 selected 候选, 返回需要写到 PTY 的同步字节
    /// (\x7f×旧 token 字符数 + replacement) 让 zsh prompt 跟 buffer 同步.
    /// 接受后清候选关 popup.
    fn accept_selected_with_diff(&mut self) -> Vec<u8> {
        let bytes = self.sync_selected_to_buffer();
        self.clear_candidates();
        bytes
    }

    /// MC 风格 selected 同步: 把当前 selected 候选 replace 当前 token 到 buffer,
    /// 返回 PTY diff 字节 (\x7f×旧 + 新 replacement). 跟 accept 区别 — 不清候选,
    /// popup 还在, 用户可以继续 Up/Down/Tab cycle.
    fn sync_selected_to_buffer(&mut self) -> Vec<u8> {
        let Some(idx) = self.selected else {
            return Vec::new();
        };
        let Some(suggestion) = self.candidates.get(idx) else {
            return Vec::new();
        };
        let replacement = suggestion.text.clone();
        let tokenized = self.current_tokenization();
        if let Some(token_idx) = tokenized.current_token_idx {
            let token = &tokenized.tokens[token_idx];
            let old_chars = self.buffer[token.start..token.end].chars().count();
            self.buffer
                .replace_range(token.start..token.end, &replacement);
            self.cursor = token.start + replacement.len();
            let mut bytes = vec![0x7fu8; old_chars];
            bytes.extend_from_slice(replacement.as_bytes());
            bytes
        } else {
            self.buffer.insert_str(self.cursor, &replacement);
            self.cursor += replacement.len();
            replacement.into_bytes()
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
        // sudo/doas/env/... 等透明前缀: provider 应该看到 effective 命令
        // (sudo pacman -S → pacman -S), 否则 PacmanProvider 不触发.
        const COMMAND_PREFIX_TRANSPARENTS: &[&str] =
            &["sudo", "doas", "env", "time", "nice", "ionice"];

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
        // 跳过 transparent 前缀, 真 command 是后面的第一个 word
        let prefix_skip = words
            .iter()
            .take_while(|(_, token)| {
                COMMAND_PREFIX_TRANSPARENTS.contains(&token.text.as_str())
            })
            .count();
        let effective_words = &words[prefix_skip..];
        let command = effective_words
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
        let previous_tokens = effective_words
            .iter()
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
    state_test!(test_char_insert_at_cursor, { let mut state = active_state(); type_chars(&mut state, "ab"); assert_eq!(state.buffer(), "ab"); assert_eq!(state.cursor(), 2); });
    state_test!(test_backspace_deletes_prev_char, { let mut state = active_state(); type_chars(&mut state, "ab"); state.handle_input(Backspace); assert_eq!(state.buffer(), "a"); assert_eq!(state.cursor(), 1); });
    state_test!(test_backspace_handles_multibyte, { let mut state = active_state(); type_chars(&mut state, "a你"); state.handle_input(Backspace); assert_eq!(state.buffer(), "a"); assert_eq!(state.cursor(), 1); });
    state_test!(test_arrow_passthrough, { let mut state = active_state(); type_chars(&mut state, "ab"); assert_eq!(state.handle_input(LeftArrow), Passthrough); assert_eq!(state.handle_input(RightArrow), Passthrough); assert_eq!(state.handle_input(Home), Passthrough); assert_eq!(state.handle_input(End), Passthrough); });
    state_test!(test_enter_passthrough_when_no_popup, { let mut state = active_state(); type_chars(&mut state, "ls"); assert_eq!(state.handle_input(Enter), Passthrough); assert_eq!(state.buffer(), "ls"); });
    state_test!(test_escape_passthrough_when_no_popup, { let mut state = active_state(); type_chars(&mut state, "x"); assert_eq!(state.handle_input(Escape), Passthrough); assert_eq!(state.buffer(), "x"); });
    state_test!(test_pending_gen_increments_on_keystroke, { let mut state = active_state(); assert_eq!(state.pending_gen, GenerationId(0)); state.handle_input(Char('a')); state.handle_input(Backspace); assert_eq!(state.pending_gen, GenerationId(2)); });
    state_test!(test_pty_segments_passthrough_non_133, { let mut state = ComposerState::new(); assert_eq!(state.feed_pty_output(b"abc\x1b]0;title\x07def"), vec![Segment::Bytes(b"abc\x1b]0;title\x07def")]); });
    state_test!(test_tab_first_press_syncs_without_cycling, { let mut state = active_state(); type_chars(&mut state, "a"); state.candidates = vec![crate::completion::Suggestion { text: "alpha".into(), display: "alpha".into(), description: String::new(), group: crate::completion::SuggestionGroup::Dynamic }, crate::completion::Suggestion { text: "beta".into(), display: "beta".into(), description: String::new(), group: crate::completion::SuggestionGroup::Dynamic }]; state.selected = Some(0); let outcome = state.handle_input(Tab); assert!(matches!(outcome, WritePty(_))); assert_eq!(state.selected(), Some(0)); assert_eq!(state.buffer(), "alpha"); let outcome2 = state.handle_input(Tab); assert!(matches!(outcome2, WritePty(_))); assert_eq!(state.selected(), Some(1)); assert_eq!(state.buffer(), "beta"); });
    state_test!(test_enter_with_popup_writes_pty, { let mut state = active_state(); type_chars(&mut state, "ch"); state.candidates = vec![crate::completion::Suggestion { text: "checkout".into(), display: "checkout".into(), description: String::new(), group: crate::completion::SuggestionGroup::Subcommand }]; state.selected = Some(0); let outcome = state.handle_input(Enter); assert!(matches!(outcome, WritePty(_))); assert_eq!(state.buffer(), "checkout"); });
}
