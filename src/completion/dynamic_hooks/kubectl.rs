use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct KubectlProvider {
    cache: super::ExternalCache,
    ttl: Duration,
}

impl KubectlProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(5),
        }
    }
}

impl Default for KubectlProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for KubectlProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        let Some(kind) = kubectl_kind(&ctx) else {
            return Ok(Vec::new());
        };

        let signature = external_cache_signature(&ctx, "kubectl", &kind);
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(names) = matching_cache_values(values, &signature) {
                return Ok(name_suggestions(&names, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-kubectl-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            "kubectl",
            vec![
                "get".to_string(),
                kind,
                "-o".to_string(),
                "name".to_string(),
            ],
            signature,
            parse_kubectl_names,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &'static str {
        "kubectl"
    }
}

fn kubectl_kind(ctx: &QueryCtx) -> Option<String> {
    let tokens = ctx_tokens(ctx);
    (tokens.first()? == "kubectl" && tokens.get(1)? == "get").then(|| tokens.get(2).cloned())?
}

fn name_suggestions(names: &[String], prefix: &str) -> Vec<Suggestion> {
    names
        .iter()
        .filter(|name| name.starts_with(prefix))
        .map(|name| dynamic_suggestion(name.clone(), name.clone(), String::new()))
        .collect()
}

fn parse_kubectl_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once('/')
                .map(|(_, name)| name)
                .unwrap_or(line)
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kubectl_strips_kind_prefix() {
        let names = parse_kubectl_names("pod/api-0\nservice/api\ndeployment.apps/web\n");

        assert_eq!(names, vec!["api-0", "api", "web"]);
        let suggestions = name_suggestions(&names, "api");
        assert_eq!(suggestions.len(), 2);
    }
}
