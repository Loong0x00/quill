const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const OSC_133_PREFIX: &[u8] = b"\x1b]133;";
const MAX_PARTIAL: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc133Event {
    PromptStart,
    InputStart,
    CommandStart,
    CommandDone(Option<i32>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    Bytes(&'a [u8]),
    Marker(Osc133Event),
}

pub struct Osc133Scanner {
    partial: Vec<u8>,
    scratch: Vec<u8>,
}

impl Osc133Scanner {
    pub fn new() -> Self {
        Self {
            partial: Vec::new(),
            scratch: Vec::new(),
        }
    }

    pub fn feed<'a>(&'a mut self, bytes: &'a [u8]) -> Vec<Segment<'a>> {
        if self.partial.is_empty() {
            let mut out = Vec::new();
            scan_source(bytes, &mut self.partial, &mut out);
            return out;
        }

        self.scratch.clear();
        self.scratch.extend_from_slice(&self.partial);
        self.scratch.extend_from_slice(bytes);
        self.partial.clear();

        let mut out = Vec::new();
        scan_source(&self.scratch, &mut self.partial, &mut out);
        out
    }
}

impl Default for Osc133Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn scan_source<'a>(source: &'a [u8], partial: &mut Vec<u8>, out: &mut Vec<Segment<'a>>) {
    let mut cursor = 0;
    let mut bytes_start = 0;

    while cursor < source.len() {
        if source[cursor] != ESC {
            cursor += 1;
            continue;
        }

        let candidate = &source[cursor..];
        if is_incomplete_osc_133_prefix(candidate) {
            buffer_or_passthrough(source, cursor, bytes_start, partial, out);
            return;
        }

        if !candidate.starts_with(b"\x1b]") {
            cursor += 1;
            continue;
        }

        if !candidate.starts_with(OSC_133_PREFIX) {
            cursor = skip_non_133_osc(source, cursor);
            continue;
        }

        let payload_start = cursor + OSC_133_PREFIX.len();
        let Some((terminator_start, terminator_end)) = find_terminator(source, payload_start)
        else {
            buffer_or_passthrough(source, cursor, bytes_start, partial, out);
            return;
        };

        let payload = &source[payload_start..terminator_start];
        let Some(event) = parse_payload(payload) else {
            cursor = terminator_end;
            continue;
        };

        push_bytes(out, &source[bytes_start..cursor]);
        out.push(Segment::Marker(event));
        cursor = terminator_end;
        bytes_start = terminator_end;
    }

    push_bytes(out, &source[bytes_start..]);
}

fn is_incomplete_osc_133_prefix(candidate: &[u8]) -> bool {
    candidate.len() < OSC_133_PREFIX.len() && OSC_133_PREFIX.starts_with(candidate)
}

fn buffer_or_passthrough<'a>(
    source: &'a [u8],
    cursor: usize,
    bytes_start: usize,
    partial: &mut Vec<u8>,
    out: &mut Vec<Segment<'a>>,
) {
    let pending = &source[cursor..];
    if pending.len() > MAX_PARTIAL {
        // 超过保护上限时不再等待终止符,按普通字节交给终端状态机。
        push_bytes(out, &source[bytes_start..]);
        partial.clear();
        return;
    }

    push_bytes(out, &source[bytes_start..cursor]);
    partial.clear();
    partial.extend_from_slice(pending);
}

fn push_bytes<'a>(out: &mut Vec<Segment<'a>>, bytes: &'a [u8]) {
    if bytes.is_empty() {
        return;
    }

    out.push(Segment::Bytes(bytes));
}

fn skip_non_133_osc(source: &[u8], start: usize) -> usize {
    let payload_start = start + 2;
    match find_terminator(source, payload_start) {
        Some((_, terminator_end)) => terminator_end,
        None => source.len(),
    }
}

fn find_terminator(source: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut cursor = start;
    while cursor < source.len() {
        match source[cursor] {
            BEL => return Some((cursor, cursor + 1)),
            ESC if cursor + 1 < source.len() && source[cursor + 1] == b'\\' => {
                return Some((cursor, cursor + 2));
            }
            _ => cursor += 1,
        }
    }
    None
}

fn parse_payload(payload: &[u8]) -> Option<Osc133Event> {
    match payload {
        b"A" => Some(Osc133Event::PromptStart),
        b"B" => Some(Osc133Event::InputStart),
        b"C" => Some(Osc133Event::CommandStart),
        b"D" => Some(Osc133Event::CommandDone(None)),
        [b'D', b';', code @ ..] => parse_exit_code(code)
            .map(Some)
            .map(Osc133Event::CommandDone),
        _ => None,
    }
}

fn parse_exit_code(code: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(code).ok()?;
    text.parse::<i32>().ok()
}

#[cfg(test)]
mod tests {
    use super::{Osc133Event, Osc133Scanner, Segment};

    #[test]
    fn test_simple_prompt_start() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(
            scanner.feed(b"\x1b]133;A\x07hello"),
            vec![
                Segment::Marker(Osc133Event::PromptStart),
                Segment::Bytes(b"hello"),
            ]
        );
    }

    #[test]
    fn test_st_terminator() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(
            scanner.feed(b"\x1b]133;A\x1b\\hello"),
            vec![
                Segment::Marker(Osc133Event::PromptStart),
                Segment::Bytes(b"hello"),
            ]
        );
    }

    #[test]
    fn test_command_done_with_exit() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(
            scanner.feed(b"\x1b]133;D;0\x07"),
            vec![Segment::Marker(Osc133Event::CommandDone(Some(0)))]
        );
    }

    #[test]
    fn test_command_done_no_exit() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(
            scanner.feed(b"\x1b]133;D\x07"),
            vec![Segment::Marker(Osc133Event::CommandDone(None))]
        );
    }

    #[test]
    fn test_split_across_reads() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(scanner.feed(b"\x1b]13"), Vec::<Segment<'_>>::new());
        assert_eq!(
            scanner.feed(b"3;A\x07x"),
            vec![
                Segment::Marker(Osc133Event::PromptStart),
                Segment::Bytes(b"x"),
            ]
        );
    }

    #[test]
    fn test_non_133_osc_passthrough() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(
            scanner.feed(b"\x1b]0;title\x07"),
            vec![Segment::Bytes(b"\x1b]0;title\x07")]
        );
    }

    #[test]
    fn test_mixed_text_and_marker() {
        let mut scanner = Osc133Scanner::new();
        assert_eq!(
            scanner.feed(b"abc\x1b]133;A\x07def"),
            vec![
                Segment::Bytes(b"abc"),
                Segment::Marker(Osc133Event::PromptStart),
                Segment::Bytes(b"def"),
            ]
        );
    }

    #[test]
    fn test_partial_buffer_overflow_protection() {
        let mut scanner = Osc133Scanner::new();
        let chunk = vec![b'x'; 1024];
        let mut saw_passthrough = false;

        assert_eq!(scanner.feed(b"\x1b]133;A"), Vec::<Segment<'_>>::new());
        for _ in 0..100 {
            let segments = scanner.feed(&chunk);
            if !segments.is_empty() {
                saw_passthrough = true;
                assert!(matches!(segments.as_slice(), [Segment::Bytes(_)]));
            }
        }

        assert!(saw_passthrough);
    }
}
