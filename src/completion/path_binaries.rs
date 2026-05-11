use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion, SuggestionGroup};
use std::collections::BTreeSet;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH_TTL: Duration = Duration::from_secs(60);
const MAX_SUGGESTIONS: usize = 20;

pub struct PathBinariesProvider {
    cache: Arc<Mutex<Option<CachedBinaries>>>,
}

struct CachedBinaries {
    names: Vec<String>,
    refreshed_at: Instant,
}

impl PathBinariesProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
        }
    }

    fn scan_path() -> Vec<String> {
        let path = env::var_os("PATH").unwrap_or_default();
        let mut names: BTreeSet<String> = BTreeSet::new();
        for dir in env::split_paths(&path) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_file() && !file_type.is_symlink() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    names.insert(name.to_string());
                }
            }
        }
        names.into_iter().collect()
    }

    fn names(&self) -> Vec<String> {
        let mut guard = self.cache.lock().unwrap();
        let need_refresh = match guard.as_ref() {
            Some(cached) => cached.refreshed_at.elapsed() > REFRESH_TTL,
            None => true,
        };
        if need_refresh {
            let names = Self::scan_path();
            *guard = Some(CachedBinaries {
                names: names.clone(),
                refreshed_at: Instant::now(),
            });
            names
        } else {
            guard.as_ref().unwrap().names.clone()
        }
    }
}

#[async_trait::async_trait]
impl Provider for PathBinariesProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !ctx.previous_tokens.is_empty() {
            return Ok(Vec::new());
        }
        let prefix = ctx.current_token.as_str();
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let names = self.names();
        let suggestions: Vec<Suggestion> = names
            .into_iter()
            .filter(|n| n.starts_with(prefix))
            .take(MAX_SUGGESTIONS)
            .map(|name| Suggestion {
                text: name.clone(),
                display: name,
                description: String::new(),
                group: SuggestionGroup::Subcommand,
            })
            .collect();
        Ok(suggestions)
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &'static str {
        "path_binaries"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(token: &str) -> QueryCtx {
        QueryCtx {
            command: token.to_string(),
            current_token: token.to_string(),
            previous_tokens: Vec::new(),
            working_dir: PathBuf::from("/"),
        }
    }

    #[test]
    fn empty_prefix_returns_nothing() {
        let provider = PathBinariesProvider::new();
        let result = futures::executor::block_on(provider.query(ctx(""), GenerationId(1))).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn second_token_returns_nothing() {
        let provider = PathBinariesProvider::new();
        let mut c = ctx("foo");
        c.previous_tokens = vec!["ls".to_string()];
        let result = futures::executor::block_on(provider.query(c, GenerationId(1))).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn ls_prefix_finds_ls() {
        let provider = PathBinariesProvider::new();
        let result =
            futures::executor::block_on(provider.query(ctx("ls"), GenerationId(1))).unwrap();
        assert!(
            result.iter().any(|s| s.text == "ls"),
            "expected `ls` in suggestions, got {:?}",
            result.iter().map(|s| &s.text).collect::<Vec<_>>()
        );
    }
}
