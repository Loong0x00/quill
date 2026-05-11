use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct GitStatusProvider {
    cache: Arc<Mutex<Option<(Vec<String>, Instant)>>>,
    ttl: Duration,
}

impl GitStatusProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(2),
        }
    }
}

impl Default for GitStatusProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for GitStatusProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !is_git_add_query(&ctx) {
            return Ok(Vec::new());
        }

        let signature = external_cache_signature(&ctx, "git_status", "paths");
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(paths) = matching_cache_values(values, &signature) {
                return Ok(path_suggestions(&paths, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-git-status-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            "git",
            vec![
                "status".to_string(),
                "--porcelain".to_string(),
                "--short".to_string(),
            ],
            signature,
            parse_git_status_paths,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "git_status"
    }
}

fn is_git_add_query(ctx: &QueryCtx) -> bool {
    let tokens = ctx_tokens(ctx);
    tokens.first().is_some_and(|token| token == "git")
        && tokens.iter().skip(1).any(|token| token == "add")
}

fn path_suggestions(paths: &[String], prefix: &str) -> Vec<Suggestion> {
    paths
        .iter()
        .filter(|path| path.starts_with(prefix))
        .map(|path| dynamic_suggestion(path.clone(), path.clone(), String::new()))
        .collect()
}

fn parse_git_status_paths(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            if line.starts_with("??") {
                return None;
            }
            let path = line.get(3..)?.trim();
            if path.is_empty() {
                return None;
            }
            let path = path
                .rsplit_once(" -> ")
                .map(|(_, new_path)| new_path)
                .unwrap_or(path);
            Some(strip_git_quotes(path).to_string())
        })
        .collect()
}

fn strip_git_quotes(path: &str) -> &str {
    path.strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_status_parses_tracked_paths_and_skips_untracked() {
        let paths = parse_git_status_paths(
            " M src/main.rs\n?? scratch.txt\nR  old.rs -> src/new.rs\nA  \"quoted.rs\"\n",
        );

        assert_eq!(paths, vec!["src/main.rs", "src/new.rs", "quoted.rs"]);
        let suggestions = path_suggestions(&paths, "src/");
        assert_eq!(suggestions.len(), 2);
    }
}
