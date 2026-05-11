pub mod cd;
pub mod readdir;
pub mod ssh;

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use cd::CdProvider;
pub use readdir::ReaddirProvider;
pub use ssh::SshProvider;

use crate::completion::{ProviderErr, ProviderRegistry, Suggestion, SuggestionGroup};

#[cfg(test)]
use crate::completion::QueryCtx;

pub fn register_local_hooks(registry: &mut ProviderRegistry) {
    registry.register(Arc::new(CdProvider));
    registry.register(Arc::new(SshProvider::new(default_ssh_config_path())));
    registry.register(Arc::new(ReaddirProvider::new_default()));
}

pub fn default_ssh_config_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".ssh/config")
}

pub(crate) struct PathPrefix {
    pub read_dir: PathBuf,
    pub prefix: String,
    pub text_prefix: String,
}

pub(crate) fn path_prefix(working_dir: &Path, token: &str) -> PathPrefix {
    let (dir_token, prefix, text_prefix) = match token.rsplit_once('/') {
        Some((dir_part, prefix)) => {
            let dir_token = if dir_part.is_empty() && token.starts_with('/') {
                "/"
            } else if dir_part.is_empty() {
                "."
            } else {
                dir_part
            };
            let text_prefix = token[..token.len() - prefix.len()].to_string();
            (dir_token.to_string(), prefix.to_string(), text_prefix)
        }
        None => (".".to_string(), token.to_string(), String::new()),
    };

    PathPrefix {
        read_dir: expand_path(working_dir, &dir_token),
        prefix,
        text_prefix,
    }
}

pub(crate) fn file_suggestion(text: String, display: String) -> Suggestion {
    Suggestion {
        text,
        display,
        description: String::new(),
        group: SuggestionGroup::File,
    }
}

pub(crate) fn read_dir(path: &Path) -> Result<Option<fs::ReadDir>, ProviderErr> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ProviderErr::Io(err.to_string())),
    }
}

fn expand_path(working_dir: &Path, path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| working_dir.to_path_buf());
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| working_dir.join(path));
    }

    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

#[cfg(test)]
pub(crate) fn test_temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!(
        "quill-dynamic-hooks-{name}-{}-{stamp}",
        std::process::id()
    ))
}

#[cfg(test)]
pub(crate) fn test_query_ctx(command: &str, working_dir: &Path, current_token: &str) -> QueryCtx {
    QueryCtx {
        command: command.to_string(),
        current_token: current_token.to_string(),
        previous_tokens: vec![command.to_string()],
        working_dir: working_dir.to_path_buf(),
    }
}

#[cfg(test)]
pub(crate) fn test_texts(suggestions: &[Suggestion]) -> Vec<&str> {
    suggestions
        .iter()
        .map(|suggestion| suggestion.text.as_str())
        .collect()
}
