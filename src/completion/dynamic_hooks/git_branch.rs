use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct GitBranchProvider {
    cache: super::ExternalCache,
    ttl: Duration,
}

impl GitBranchProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(5),
        }
    }
}

impl Default for GitBranchProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for GitBranchProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !is_git_branch_query(&ctx) {
            return Ok(Vec::new());
        }

        let signature = external_cache_signature(&ctx, "git_branch", "branches");
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(branches) = matching_cache_values(values, &signature) {
                return Ok(branch_suggestions(&branches, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-git-branch-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            "git",
            vec![
                "branch".to_string(),
                "--list".to_string(),
                "--sort=-committerdate".to_string(),
                "--format=%(refname:short)".to_string(),
            ],
            signature,
            parse_git_branches,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &'static str {
        "git_branch"
    }
}

fn is_git_branch_query(ctx: &QueryCtx) -> bool {
    let tokens = ctx_tokens(ctx);
    tokens.first().is_some_and(|token| token == "git")
        && tokens
            .iter()
            .skip(1)
            .any(|token| token == "checkout" || token == "switch")
}

fn branch_suggestions(branches: &[String], prefix: &str) -> Vec<Suggestion> {
    branches
        .iter()
        .filter(|branch| branch.starts_with(prefix))
        .map(|branch| dynamic_suggestion(branch.clone(), branch.clone(), String::new()))
        .collect()
}

fn parse_git_branches(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_branch_parses_and_filters_branches() {
        let branches = parse_git_branches("main\n feature/login \n\nrelease\n");

        assert_eq!(branches, vec!["main", "feature/login", "release"]);
        let suggestions = branch_suggestions(&branches, "fea");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "feature/login");
    }
}
