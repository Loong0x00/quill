use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct PacmanProvider {
    cache: Arc<Mutex<Option<(Vec<String>, Instant)>>>,
    ttl: Duration,
}

impl PacmanProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(600),
        }
    }
}

impl Default for PacmanProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for PacmanProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        let Some(program) = pacman_program(&ctx) else {
            return Ok(Vec::new());
        };

        let signature = external_cache_signature(&ctx, "pacman", &program);
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(packages) = matching_cache_values(values, &signature) {
                return Ok(package_suggestions(&packages, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-pacman-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            &program,
            vec!["-Slq".to_string()],
            signature,
            parse_packages,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "pacman"
    }
}

fn pacman_program(ctx: &QueryCtx) -> Option<String> {
    let tokens = ctx_tokens(ctx);
    let command = tokens.first()?;
    matches!(command.as_str(), "pacman" | "yay")
        .then(|| tokens.iter().skip(1).any(|token| token == "-S"))
        .and_then(|has_sync| has_sync.then(|| command.clone()))
}

fn package_suggestions(packages: &[String], prefix: &str) -> Vec<Suggestion> {
    packages
        .iter()
        .filter(|package| package.starts_with(prefix))
        .map(|package| dynamic_suggestion(package.clone(), package.clone(), String::new()))
        .collect()
}

fn parse_packages(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pacman_parses_package_list() {
        let packages = parse_packages("ripgrep\n fd \n\nlinux\n");

        assert_eq!(packages, vec!["ripgrep", "fd", "linux"]);
        let suggestions = package_suggestions(&packages, "r");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "ripgrep");
    }
}
