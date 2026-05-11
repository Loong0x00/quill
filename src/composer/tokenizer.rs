//! shell风格tokenizer，用于composer buffer解析。ASCII优先，复杂unicode归Word。

#[derive(Debug, Clone, PartialEq)]
pub enum RedirectKind {
    Out,
    Append,
    In,
    Heredoc,
    ErrTo,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Word,
    Pipe,
    Redirect(RedirectKind),
    Background,
    Sequence,
    Unterminated,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub text: String, // unescape后的实际文本
    pub raw: String,  // buffer里的原始文本(含引号/转义)
    pub start: usize, // raw在buffer里的起始字节位置
    pub end: usize,   // raw在buffer里的结束字节位置(exclusive)
    pub kind: TokenKind,
}

#[derive(Debug)]
pub struct Tokenized {
    pub tokens: Vec<Token>,
    pub current_token_idx: Option<usize>, // 光标所在token下标
    pub current_token_offset: usize,      // 光标在该token内的字节偏移
}

/// 解析shell命令buffer，返回token列表及光标定位信息
pub fn tokenize(buffer: &str, cursor: usize) -> Tokenized {
    let b = buffer.as_bytes();
    let n = b.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0usize;

    while i < n {
        if b[i] == b' ' || b[i] == b'\t' {
            i += 1;
            continue;
        }
        let start = i;

        // 2> 或 &> 重定向前缀
        if (b[i] == b'2' || b[i] == b'&') && i + 1 < n && b[i + 1] == b'>' {
            let k = if b[i] == b'2' {
                RedirectKind::ErrTo
            } else {
                RedirectKind::Out
            };
            let raw = format!("{}>", b[i] as char);
            i += 2;
            tokens.push(Token {
                text: raw.clone(),
                raw,
                start,
                end: i,
                kind: TokenKind::Redirect(k),
            });
            continue;
        }
        // > >>
        if b[i] == b'>' {
            i += 1;
            let (k, raw): (RedirectKind, String) = if i < n && b[i] == b'>' {
                i += 1;
                (RedirectKind::Append, ">>".into())
            } else {
                (RedirectKind::Out, ">".into())
            };
            tokens.push(Token {
                text: raw.clone(),
                raw,
                start,
                end: i,
                kind: TokenKind::Redirect(k),
            });
            continue;
        }
        // < <<
        if b[i] == b'<' {
            i += 1;
            let (k, raw): (RedirectKind, String) = if i < n && b[i] == b'<' {
                i += 1;
                (RedirectKind::Heredoc, "<<".into())
            } else {
                (RedirectKind::In, "<".into())
            };
            tokens.push(Token {
                text: raw.clone(),
                raw,
                start,
                end: i,
                kind: TokenKind::Redirect(k),
            });
            continue;
        }
        // | 或 ||
        if b[i] == b'|' {
            i += 1;
            if i < n && b[i] == b'|' {
                i += 1;
                tokens.push(Token {
                    text: "||".into(),
                    raw: "||".into(),
                    start,
                    end: i,
                    kind: TokenKind::Sequence,
                });
            } else {
                tokens.push(Token {
                    text: "|".into(),
                    raw: "|".into(),
                    start,
                    end: i,
                    kind: TokenKind::Pipe,
                });
            }
            continue;
        }
        // && 或 &(background)
        if b[i] == b'&' {
            i += 1;
            if i < n && b[i] == b'&' {
                i += 1;
                tokens.push(Token {
                    text: "&&".into(),
                    raw: "&&".into(),
                    start,
                    end: i,
                    kind: TokenKind::Sequence,
                });
            } else {
                tokens.push(Token {
                    text: "&".into(),
                    raw: "&".into(),
                    start,
                    end: i,
                    kind: TokenKind::Background,
                });
            }
            continue;
        }
        // ;
        if b[i] == b';' {
            i += 1;
            tokens.push(Token {
                text: ";".into(),
                raw: ";".into(),
                start,
                end: i,
                kind: TokenKind::Sequence,
            });
            continue;
        }

        // Word或Unterminated
        let mut raw = String::new();
        let mut text = String::new();
        let mut unt = false;
        while i < n {
            match b[i] {
                c if c == b' ' || c == b'\t' => break,
                c if c == b'|' || c == b'&' || c == b';' || c == b'>' || c == b'<' => break,
                b'\'' => {
                    // 单引号字面
                    raw.push('\'');
                    i += 1;
                    while i < n && b[i] != b'\'' {
                        text.push(b[i] as char);
                        raw.push(b[i] as char);
                        i += 1;
                    }
                    if i < n {
                        raw.push('\'');
                        i += 1;
                    } else {
                        unt = true;
                        break;
                    }
                }
                b'"' => {
                    // 双引号，支持\" \\转义
                    raw.push('"');
                    i += 1;
                    while i < n && b[i] != b'"' {
                        if b[i] == b'\\' && i + 1 < n {
                            let nx = b[i + 1];
                            raw.push('\\');
                            raw.push(nx as char);
                            i += 2;
                            match nx {
                                b'"' => text.push('"'),
                                b'\\' => text.push('\\'),
                                _ => {
                                    text.push('\\');
                                    text.push(nx as char);
                                }
                            }
                        } else {
                            text.push(b[i] as char);
                            raw.push(b[i] as char);
                            i += 1;
                        }
                    }
                    if i < n {
                        raw.push('"');
                        i += 1;
                    } else {
                        unt = true;
                        break;
                    }
                }
                b'\\' if i + 1 < n => {
                    // 反斜杠转义
                    let nx = b[i + 1];
                    raw.push('\\');
                    raw.push(nx as char);
                    i += 2;
                    if nx != b'\n' {
                        text.push(nx as char);
                    }
                }
                c => {
                    text.push(c as char);
                    raw.push(c as char);
                    i += 1;
                }
            }
        }
        let kind = if unt {
            TokenKind::Unterminated
        } else {
            TokenKind::Word
        };
        tokens.push(Token {
            text,
            raw,
            start,
            end: i,
            kind,
        });
    }

    // 光标定位
    let mut idx = None;
    let mut off = 0usize;
    for (ti, tok) in tokens.iter().enumerate() {
        if cursor >= tok.start && cursor < tok.end {
            idx = Some(ti);
            off = cursor - tok.start;
            break;
        }
    }
    // cursor==buffer.len()且末尾非空白：归到最后token
    if idx.is_none() && cursor == n && n > 0 && b[n - 1] != b' ' && b[n - 1] != b'\t' {
        if let Some(last) = tokens.len().checked_sub(1) {
            if cursor == tokens[last].end {
                idx = Some(last);
                off = tokens[last].end - tokens[last].start;
            }
        }
    }
    Tokenized {
        tokens,
        current_token_idx: idx,
        current_token_offset: off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_words() {
        let t = tokenize("git checkout main", 0);
        assert_eq!(t.tokens.len(), 3);
        assert!(t.tokens.iter().all(|tk| tk.kind == TokenKind::Word));
        assert_eq!(t.tokens[2].text, "main");
    }

    #[test]
    fn test_single_quotes() {
        let t = tokenize("echo 'hello world'", 0);
        assert_eq!(t.tokens.len(), 2);
        assert_eq!(t.tokens[1].text, "hello world");
        assert_eq!(t.tokens[1].raw, "'hello world'");
    }

    #[test]
    fn test_double_quotes_with_escape() {
        let t = tokenize(r#"echo "a\"b""#, 0);
        assert_eq!(t.tokens.len(), 2);
        assert_eq!(t.tokens[1].text, r#"a"b"#);
    }

    #[test]
    fn test_backslash_space() {
        let t = tokenize(r"cd path\ with\ spaces", 0);
        assert_eq!(t.tokens.len(), 2);
        assert_eq!(t.tokens[1].text, "path with spaces");
    }

    #[test]
    fn test_pipe_simple() {
        let t = tokenize("ls | grep foo", 0);
        assert_eq!(t.tokens.len(), 4);
        assert_eq!(t.tokens[1].kind, TokenKind::Pipe);
        assert!(matches!(t.tokens[0].kind, TokenKind::Word));
    }

    #[test]
    fn test_redirect_out() {
        let t = tokenize("echo hi > file.txt", 0);
        assert_eq!(t.tokens.len(), 4);
        assert_eq!(t.tokens[2].kind, TokenKind::Redirect(RedirectKind::Out));
        assert_eq!(t.tokens[3].text, "file.txt");
    }

    #[test]
    fn test_redirect_append() {
        let t = tokenize("echo hi >> file", 0);
        assert_eq!(t.tokens.len(), 4);
        assert_eq!(t.tokens[2].kind, TokenKind::Redirect(RedirectKind::Append));
    }

    #[test]
    fn test_sequence_and() {
        let t = tokenize("a && b", 0);
        assert_eq!(t.tokens.len(), 3);
        assert_eq!(t.tokens[1].kind, TokenKind::Sequence);
        assert_eq!(t.tokens[1].raw, "&&");
    }

    #[test]
    fn test_unterminated_quote() {
        let t = tokenize("echo 'hello", 0);
        assert_eq!(t.tokens.len(), 2);
        assert_eq!(t.tokens[1].kind, TokenKind::Unterminated);
        assert_eq!(t.tokens[1].text, "hello");
    }

    #[test]
    fn test_cursor_in_middle() {
        let t = tokenize("git ch", 6);
        assert_eq!(t.current_token_idx, Some(1));
        assert_eq!(t.current_token_offset, 2);
    }

    #[test]
    fn test_cursor_in_whitespace() {
        let t = tokenize("git  checkout", 4);
        assert_eq!(t.current_token_idx, None);
    }

    #[test]
    fn test_cursor_at_end_no_trailing_space() {
        let t = tokenize("git ch", 6);
        assert_eq!(t.current_token_idx, Some(1));
    }

    #[test]
    fn test_cursor_at_end_after_space() {
        let t = tokenize("git ", 4);
        assert_eq!(t.current_token_idx, None);
    }

    #[test]
    fn test_pipe_at_cursor() {
        // "ls | gr" tokens: ls(0) |(1) gr(2)，cursor=7在gr末尾
        let t = tokenize("ls | gr", 7);
        assert_eq!(t.current_token_idx, Some(2));
        assert_eq!(t.current_token_offset, 2);
    }
}
