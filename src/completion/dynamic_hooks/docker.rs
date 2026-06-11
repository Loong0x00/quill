use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct DockerProvider {
    cache: super::ExternalCache,
    ttl: Duration,
}

impl DockerProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(5),
        }
    }
}

impl Default for DockerProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for DockerProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !is_docker_query(&ctx) {
            return Ok(Vec::new());
        }

        let signature = external_cache_signature(&ctx, "docker", "containers");
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(containers) = matching_cache_values(values, &signature) {
                return Ok(container_suggestions(&containers, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-docker-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            "docker",
            vec![
                "ps".to_string(),
                "--format".to_string(),
                "{{.Names}}\t{{.Image}}".to_string(),
            ],
            signature,
            parse_docker_containers,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &'static str {
        "docker"
    }
}

fn is_docker_query(ctx: &QueryCtx) -> bool {
    let tokens = ctx_tokens(ctx);
    tokens.first().is_some_and(|token| token == "docker")
        && tokens
            .iter()
            .skip(1)
            .any(|token| matches!(token.as_str(), "run" | "exec" | "stop" | "logs" | "inspect"))
}

fn container_suggestions(containers: &[String], prefix: &str) -> Vec<Suggestion> {
    containers
        .iter()
        .filter_map(|container| {
            let (name, image) = container.split_once('\t').unwrap_or((container, ""));
            name.starts_with(prefix)
                .then(|| dynamic_suggestion(name.to_string(), name.to_string(), image.to_string()))
        })
        .collect()
}

fn parse_docker_containers(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docker_parses_name_image_rows() {
        let containers = parse_docker_containers("api\talpine:3\n\npostgres\tpostgres:16\n");

        assert_eq!(containers, vec!["api\talpine:3", "postgres\tpostgres:16"]);
        let suggestions = container_suggestions(&containers, "api");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "api");
        assert_eq!(suggestions[0].description, "alpine:3");
    }
}
