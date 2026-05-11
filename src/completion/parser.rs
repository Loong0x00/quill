use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::completion::{Suggestion, SuggestionGroup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[rustfmt::skip]
pub enum HelpFormat { #[serde(alias = "clap")] Clap, #[serde(alias = "argparse")] Argparse, #[serde(alias = "docopt")] Docopt, #[serde(alias = "unknown")] Unknown }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[rustfmt::skip]
pub struct ParsedHelp { pub format: HelpFormat, pub flags: Vec<FlagSpec>, pub subcommands: Vec<SubcommandSpec>, pub positional_args: Vec<ArgSpec> }

#[rustfmt::skip]
impl ParsedHelp {
    fn empty(format: HelpFormat) -> Self { Self { format, flags: Vec::new(), subcommands: Vec::new(), positional_args: Vec::new() } }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[rustfmt::skip]
pub struct FlagSpec { pub short: Option<String>, pub long: Option<String>, pub takes_value: bool, pub value_name: Option<String>, pub description: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[rustfmt::skip]
pub struct SubcommandSpec { pub name: String, pub description: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[rustfmt::skip]
pub struct ArgSpec { pub name: String, pub description: String, pub required: bool }

#[derive(Debug, Clone, PartialEq, Eq)]
#[rustfmt::skip]
pub struct ParseErr { pub message: String }

pub fn parse(help_output: &str) -> ParsedHelp {
    // GNU coreutils 新版 --help 输出含 OSC 8 hyperlink + ANSI bold (尤其
    // ls / pacman / 部分国际化版本), 让 line.starts_with('-') 失败导致解析 0 flag.
    // 入口 strip 所有 ANSI escape, 不影响纯文本路径.
    let cleaned = strip_ansi(help_output);
    let text = cleaned.as_str();
    let mut parsed = match detect_format(text) {
        HelpFormat::Clap => parse_clap(text),
        HelpFormat::Argparse => parse_argparse(text),
        HelpFormat::Docopt => parse_docopt(text),
        HelpFormat::Unknown => parse_heuristic(text),
    };
    // 兜底: 抓所有 `{-X --xxx}` / `{-X|--xxx}` 大括号内的 flag (pacman 风格 op flag).
    // 不重复 push (parse_*_*  已用 seen_flags 排重).
    extract_brace_flags(text, &mut parsed);
    parsed
}

fn extract_brace_flags(text: &str, parsed: &mut ParsedHelp) {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\{(-[A-Za-z0-9][^}]*)\}").expect("brace flag regex must compile")
    });
    let mut seen: std::collections::HashSet<(Option<String>, Option<String>)> = parsed
        .flags
        .iter()
        .map(|f| (f.short.clone(), f.long.clone()))
        .collect();
    for cap in re.captures_iter(text) {
        let inside = &cap[1];
        // 抓所有 -X 跟 --xxx
        let tokens: Vec<&str> = inside
            .split(|c: char| c == ',' || c == '|' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .collect();
        let short = tokens
            .iter()
            .copied()
            .find(|t| t.starts_with('-') && !t.starts_with("--") && t.len() == 2)
            .map(|t| t.to_string());
        let long = tokens
            .iter()
            .copied()
            .find(|t| t.starts_with("--") && t.len() > 2)
            .map(|t| t.to_string());
        if short.is_none() && long.is_none() {
            continue;
        }
        if seen.insert((short.clone(), long.clone())) {
            parsed.flags.push(FlagSpec {
                short,
                long,
                takes_value: false,
                value_name: None,
                description: String::new(),
            });
        }
    }
}

/// 去掉 ANSI escape: CSI (\x1b[...letter), OSC (\x1b]...\x07 或 \x1b]...\x1b\\),
/// 单字符 (\x1b 后跟字母数字单字符). 简化版, 不严格解析全部 escape, 但能搞掉
/// `--help` 输出常见的颜色/超链接.
fn strip_ansi(input: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][\x20-\x7e]",
        )
        .expect("strip_ansi regex must compile")
    });
    re.replace_all(input, "").into_owned()
}

pub fn to_suggestions(parsed: &ParsedHelp, current_token: &str) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |text: &str, description: &str, group| {
        if (current_token.is_empty() || text.starts_with(current_token))
            && seen.insert(text.to_string())
        {
            suggestions.push(Suggestion {
                text: text.to_string(),
                display: text.to_string(),
                description: description.to_string(),
                group,
            });
        }
    };
    for flag in &parsed.flags {
        for text in [flag.short.as_deref(), flag.long.as_deref()]
            .into_iter()
            .flatten()
        {
            push(text, &flag.description, SuggestionGroup::Flag);
        }
    }
    for subcommand in &parsed.subcommands {
        push(
            &subcommand.name,
            &subcommand.description,
            SuggestionGroup::Subcommand,
        );
    }
    for arg in &parsed.positional_args {
        push(&arg.name, &arg.description, SuggestionGroup::Dynamic);
    }
    suggestions
}

pub fn validate_json(json: &str) -> Result<ParsedHelp, ParseErr> {
    let parsed = serde_json::from_str(json).map_err(|err| parse_err(err.to_string()))?;
    validate_parsed_help(&parsed)?;
    Ok(parsed)
}

fn detect_format(help_output: &str) -> HelpFormat {
    static DOCOPT_USAGE_RE: OnceLock<Regex> = OnceLock::new();
    let docopt_usage_re = DOCOPT_USAGE_RE.get_or_init(|| {
        Regex::new(r"(?m)^Usage:\s*\n\s+\S").expect("docopt usage regex must compile")
    });
    if docopt_usage_re.is_match(help_output) {
        return HelpFormat::Docopt;
    }

    let lower = help_output.to_ascii_lowercase();
    if lower.contains("optional arguments:")
        || lower.contains("positional arguments:")
        || (help_output
            .lines()
            .any(|line| line.trim_start().starts_with("usage:"))
            && help_output.contains("\noptions:"))
    {
        return HelpFormat::Argparse;
    }
    if has_options_section(help_output) && help_output.lines().any(line_has_short_option) {
        HelpFormat::Clap
    } else {
        HelpFormat::Unknown
    }
}

const COMMAND_SECTIONS: &[&str] = &["commands", "subcommands"];

fn parse_clap(help_output: &str) -> ParsedHelp {
    parse_sectioned(
        help_output,
        HelpFormat::Clap,
        COMMAND_SECTIONS,
        &["options"],
    )
}

fn parse_argparse(help_output: &str) -> ParsedHelp {
    parse_sectioned(
        help_output,
        HelpFormat::Argparse,
        COMMAND_SECTIONS,
        &["options", "optional arguments"],
    )
}

#[rustfmt::skip]
fn parse_docopt(help_output: &str) -> ParsedHelp {
    let mut parsed = ParsedHelp::empty(HelpFormat::Docopt);
    let (mut section, mut usage_lines) = (Section::Other, Vec::new());
    let (mut seen_args, mut seen_subcommands) = (HashSet::new(), HashSet::new());
    for line in help_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { section = Section::Other; }
        else if starts_section(trimmed, "Usage") {
            section = Section::Usage;
            if let Some((_, rest)) = trimmed.split_once(':') {
                if !rest.trim().is_empty() { usage_lines.push(rest.trim().to_string()); }
            }
        } else if starts_section(trimmed, "Options") { section = Section::Options; }
        else if section == Section::Usage { usage_lines.push(trimmed.to_string()); }
        else if section == Section::Options { parsed.flags.extend(parse_flag_line(line)); }
    }
    for usage in usage_lines {
        for name in angle_values(&usage) {
            push_arg(&mut parsed.positional_args, &mut seen_args, name, String::new(), !usage.contains('['));
        }
        for token in usage.split_whitespace().skip(1).map(clean_usage_token).filter(|token| is_usage_subcommand(token)) {
            push_subcommand(&mut parsed.subcommands, &mut seen_subcommands, token.to_string(), String::new());
        }
    }
    parsed
}

fn parse_heuristic(help_output: &str) -> ParsedHelp {
    parse_sectioned(help_output, HelpFormat::Unknown, COMMAND_SECTIONS, &[])
}

#[rustfmt::skip]
fn parse_sectioned(help_output: &str, format: HelpFormat, subcommand_sections: &[&str], option_sections: &[&str]) -> ParsedHelp {
    let mut parsed = ParsedHelp::empty(format);
    let (mut section, mut last_flag) = (Section::Other, None);
    let (mut seen_flags, mut seen_subcommands, mut seen_args) = (HashSet::new(), HashSet::new(), HashSet::new());
    for line in help_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        if let Some(name) = section_name(trimmed) {
            if contains_section(subcommand_sections, name) {
                section = Section::Subcommands; last_flag = None; continue;
            }
            if contains_section(option_sections, name) {
                section = Section::Options; last_flag = None; continue;
            }
            if name.eq_ignore_ascii_case("arguments") || name.eq_ignore_ascii_case("positional arguments") {
                section = Section::Positionals; last_flag = None; continue;
            }
        }
        if let Some(flag) = parse_flag_line(line) {
            if seen_flags.insert((flag.short.clone(), flag.long.clone())) {
                parsed.flags.push(flag); last_flag = Some(parsed.flags.len() - 1);
            }
        } else if section == Section::Options && line.starts_with(char::is_whitespace) {
            if let Some(index) = last_flag {
                append_description(&mut parsed.flags[index].description, trimmed);
            }
        } else if section == Section::Subcommands {
            if let Some((name, description)) = parse_name_description(line) {
                let name = name.trim_end_matches(',').to_string();
                push_subcommand(
                    &mut parsed.subcommands,
                    &mut seen_subcommands,
                    name,
                    description,
                );
            }
        } else if section == Section::Positionals {
            if let Some((name, description)) = parse_name_description(line) {
                push_arg(&mut parsed.positional_args, &mut seen_args, name, description, true);
            }
        }
    }
    parsed
}

fn parse_flag_line(line: &str) -> Option<FlagSpec> {
    let trimmed = line.trim();
    if !trimmed.starts_with('-') {
        return None;
    }
    let (spec, description) = split_spec_description(trimmed);
    let tokens = flag_tokens(spec);
    let short = tokens
        .iter()
        .copied()
        .find(|token| token.starts_with('-') && !token.starts_with("--") && token.len() >= 2)
        .map(|token| token[..2].to_string());
    let long = tokens
        .iter()
        .copied()
        .find(|token| token.starts_with("--"))
        .map(|token| {
            token
                .split_once('=')
                .map(|(name, _)| name)
                .unwrap_or(token)
                .trim_end_matches("...")
                .to_string()
        });
    if short.is_none() && long.is_none() {
        return None;
    }
    let value_name = find_value_name(&tokens);
    Some(FlagSpec {
        short,
        long,
        takes_value: value_name.is_some(),
        value_name,
        description,
    })
}

fn find_value_name(tokens: &[&str]) -> Option<String> {
    for token in tokens {
        if let Some((_, value)) = token.split_once('=') {
            let value = clean_value_token(value);
            if is_value_token(value) {
                return Some(value.to_string());
            }
        }
    }
    tokens.windows(2).find_map(|pair| {
        let value = clean_value_token(pair[1]);
        (pair[0].starts_with('-') && is_value_token(value)).then(|| value.to_string())
    })
}

fn split_spec_description(trimmed: &str) -> (&str, String) {
    let bytes = trimmed.as_bytes();
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index].is_ascii_whitespace() && bytes[index + 1].is_ascii_whitespace() {
            return (trimmed[..index].trim(), trimmed[index..].trim().to_string());
        }
    }
    (trimmed, String::new())
}

fn flag_tokens(spec: &str) -> Vec<&str> {
    spec.split(|ch: char| ch == ',' || ch.is_whitespace())
        .map(|token| token.trim_matches(|ch| ch == '[' || ch == ']' || ch == '(' || ch == ')'))
        .filter(|token| !token.is_empty() && *token != "|")
        .collect()
}

fn clean_value_token(token: &str) -> &str {
    token
        .trim_matches(|ch| ch == '[' || ch == ']' || ch == '(' || ch == ')' || ch == ',')
        .trim_end_matches("...")
}

fn is_value_token(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && (token.starts_with('<')
            || token.starts_with('{')
            || token.chars().any(|ch| ch == '_' || ch.is_ascii_uppercase()))
}

fn parse_name_description(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let (name, description) = split_spec_description(trimmed);
    (name != trimmed && !name.starts_with('-')).then(|| (name.to_string(), description))
}

#[rustfmt::skip]
fn push_subcommand(subcommands: &mut Vec<SubcommandSpec>, seen: &mut HashSet<String>, name: String, description: String) {
    if is_command_name(&name) && seen.insert(name.clone()) { subcommands.push(SubcommandSpec { name, description }); }
}

#[rustfmt::skip]
fn push_arg(args: &mut Vec<ArgSpec>, seen: &mut HashSet<String>, name: String, description: String, required: bool) {
    if seen.insert(name.clone()) { args.push(ArgSpec { name, description, required }); }
}

fn parse_err(message: impl Into<String>) -> ParseErr {
    ParseErr {
        message: message.into(),
    }
}

#[rustfmt::skip]
fn validate_parsed_help(parsed: &ParsedHelp) -> Result<(), ParseErr> {
    if parsed.flags.iter().any(|flag| flag.short.is_none() && flag.long.is_none()) {
        return Err(parse_err("flag must have short or long name"));
    }
    if parsed.flags.iter().any(|flag| flag.takes_value != flag.value_name.is_some()) {
        return Err(parse_err("flag takes_value must match value_name"));
    }
    if parsed.subcommands.iter().any(|cmd| cmd.name.is_empty()) || parsed.positional_args.iter().any(|arg| arg.name.is_empty()) {
        return Err(parse_err("parsed names must not be empty"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[rustfmt::skip]
enum Section { Other, Usage, Options, Subcommands, Positionals }

fn has_options_section(help_output: &str) -> bool {
    help_output.lines().any(|line| {
        let lower = line.trim().to_ascii_lowercase();
        lower == "flags:" || lower.ends_with("options:")
    })
}

fn line_has_short_option(line: &str) -> bool {
    flag_tokens(line.trim())
        .iter()
        .any(|token| token.starts_with('-') && !token.starts_with("--") && token.len() >= 2)
}

fn section_name(trimmed: &str) -> Option<&str> {
    trimmed.strip_suffix(':').map(str::trim)
}

#[rustfmt::skip]
fn starts_section(trimmed: &str, name: &str) -> bool {
    trimmed.get(..name.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(name)) && trimmed[name.len()..].starts_with(':')
}

fn contains_section(sections: &[&str], name: &str) -> bool {
    sections
        .iter()
        .any(|section| section.eq_ignore_ascii_case(name))
}

fn append_description(description: &mut String, continuation: &str) {
    if !continuation.is_empty() {
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str(continuation);
    }
}

fn angle_values(text: &str) -> Vec<String> {
    static ANGLE_RE: OnceLock<Regex> = OnceLock::new();
    ANGLE_RE
        .get_or_init(|| Regex::new(r"<[^>\s]+>").expect("angle value regex must compile"))
        .find_iter(text)
        .map(|matched| matched.as_str().to_string())
        .collect()
}

#[rustfmt::skip]
fn clean_usage_token(token: &str) -> &str {
    token.trim_matches(|ch| matches!(ch, '[' | ']' | '(' | ')' | '|' | ',' | '.' | ':' | '<' | '>'))
}

fn is_usage_subcommand(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains('=')
        && !token.chars().any(|ch| ch.is_ascii_uppercase())
}

fn is_command_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;

    fn texts(suggestions: &[Suggestion]) -> Vec<&str> { suggestions.iter().map(|suggestion| suggestion.text.as_str()).collect() }

    #[test] fn test_detect_clap_format() {
        assert_eq!(detect_format("Usage: git [OPTIONS] [COMMAND]\n\nOptions:\n  -h, --help  Print help\n"), HelpFormat::Clap);
    }

    #[test] fn test_detect_argparse_format() {
        assert_eq!(detect_format("usage: pip [-h]\n\noptional arguments:\n  -h, --help  show help\n"), HelpFormat::Argparse);
    }

    #[test] fn test_detect_docopt_format() {
        assert_eq!(detect_format("Usage:\n  prog ship new <name>\n\nOptions:\n  -h --help  Show help.\n"), HelpFormat::Docopt);
    }

    #[test] fn test_detect_unknown_falls_back_to_heuristic() {
        let parsed = parse("flags:\n  --json <FILE>  Emit json\n");
        assert_eq!((parsed.format, parsed.flags[0].long.as_deref()), (HelpFormat::Unknown, Some("--json")));
    }

    #[test] fn test_parse_clap_flags() {
        let parsed = parse_clap("Options:\n  -h, --help          Print help\n      --color <WHEN>  Color mode\n");
        assert_eq!(parsed.flags.len(), 2);
        assert_eq!((parsed.flags[0].short.as_deref(), parsed.flags[0].long.as_deref()), (Some("-h"), Some("--help")));
        assert_eq!((parsed.flags[1].long.as_deref(), parsed.flags[1].value_name.as_deref()), (Some("--color"), Some("<WHEN>")));
    }

    #[test] fn test_parse_clap_subcommands() {
        let parsed = parse_clap("Commands:\n  build    Compile\n  check    Check package\n\nOptions:\n  -h, --help  Print help\n");
        assert!(parsed.subcommands.iter().any(|cmd| cmd.name == "build"));
        assert_eq!(parsed.subcommands[0].description, "Compile");
    }

    #[test] fn test_parse_argparse_optional_args() {
        let parsed = parse_argparse("usage: demo [-h] [-o OUT]\n\npositional arguments:\n  file        input file\n\noptional arguments:\n  -h, --help  show help\n  -o OUT, --output OUT  write output\n");
        assert_eq!(parsed.positional_args[0].name, "file");
        assert_eq!((parsed.flags[1].short.as_deref(), parsed.flags[1].value_name.as_deref()), (Some("-o"), Some("OUT")));
    }

    #[test] fn test_parse_docopt_usage_block() {
        let parsed = parse_docopt("Usage:\n  prog ship new <name>\n  prog ship move <name> <x> <y>\n\nOptions:\n  -h --help     Show help.\n  --speed=<kn>  Speed.\n");
        assert!(parsed.subcommands.iter().any(|cmd| cmd.name == "ship"));
        assert!(parsed.positional_args.iter().any(|arg| arg.name == "<name>"));
        assert_eq!(parsed.flags[1].long.as_deref(), Some("--speed"));
    }

    #[test] fn test_to_suggestions_filters_by_prefix() {
        let parsed = parse_clap("Options:\n  --help     Print help\n  --version  Print version\n");
        assert_eq!(texts(&to_suggestions(&parsed, "--ver")), vec!["--version"]);
    }

    #[test] fn test_json_serde_roundtrip() {
        let parsed = parse_clap("Options:\n  -h, --help  Print help\n");
        assert_eq!(validate_json(&serde_json::to_string(&parsed).unwrap()).unwrap(), parsed);
    }
}
