use std::time::Instant;

use crate::composer::prompt_track::{Osc133Event, Osc133Scanner, Segment};
use crate::composer::tokenizer::{tokenize, Tokenized};

pub struct ComposerState {
    active: bool,
    buffer: String,
    cursor: usize,
    scanner: Osc133Scanner,
    in_prompt: bool,
    last_keystroke_at: Instant,
    pending_gen: u64,
    ime_preedit_active: bool,
    terminal_in_alt_screen: bool,
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
        Self {
            active: false,
            buffer: String::new(),
            cursor: 0,
            scanner: Osc133Scanner::new(),
            in_prompt: false,
            last_keystroke_at: Instant::now(),
            pending_gen: 0,
            ime_preedit_active: false,
            terminal_in_alt_screen: false,
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
                }
                Segment::Marker(Osc133Event::InputStart) => {}
                Segment::Marker(Osc133Event::CommandStart | Osc133Event::CommandDone(_)) => {
                    self.active = false;
                    self.in_prompt = false;
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
                if self.cursor < self.buffer.len() {
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
            ComposerInput::Enter => {
                let submitted = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                self.touch();
                ComposerOutcome::Submit(submitted)
            }
            ComposerInput::Escape => {
                self.active = false;
                self.buffer.clear();
                self.cursor = 0;
                self.touch();
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

    fn touch(&mut self) {
        self.pending_gen = self.pending_gen.wrapping_add(1);
        self.last_keystroke_at = Instant::now();
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
    state_test!(test_pending_gen_increments_on_keystroke, { let mut state = active_state(); assert_eq!(state.pending_gen, 0); state.handle_input(Char('a')); state.handle_input(LeftArrow); assert_eq!(state.pending_gen, 2); });
    state_test!(test_pty_segments_passthrough_non_133, { let mut state = ComposerState::new(); assert_eq!(state.feed_pty_output(b"abc\x1b]0;title\x07def"), vec![Segment::Bytes(b"abc\x1b]0;title\x07def")]); });
}
