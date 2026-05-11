use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct SystemctlProvider {
    cache: Arc<Mutex<Option<(Vec<String>, Instant)>>>,
    ttl: Duration,
}

impl SystemctlProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(30),
        }
    }
}

impl Default for SystemctlProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for SystemctlProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !is_systemctl_query(&ctx) {
            return Ok(Vec::new());
        }

        let signature = external_cache_signature(&ctx, "systemctl", "services");
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(services) = matching_cache_values(values, &signature) {
                return Ok(service_suggestions(&services, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-systemctl-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            "systemctl",
            vec![
                "list-units".to_string(),
                "--no-legend".to_string(),
                "--no-pager".to_string(),
                "--type=service".to_string(),
            ],
            signature,
            parse_services,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "systemctl"
    }
}

fn is_systemctl_query(ctx: &QueryCtx) -> bool {
    let tokens = ctx_tokens(ctx);
    tokens.first().is_some_and(|token| token == "systemctl")
        && tokens.iter().skip(1).any(|token| {
            matches!(
                token.as_str(),
                "start" | "stop" | "restart" | "status" | "enable" | "disable"
            )
        })
}

fn service_suggestions(services: &[String], prefix: &str) -> Vec<Suggestion> {
    services
        .iter()
        .filter(|service| service.starts_with(prefix))
        .map(|service| dynamic_suggestion(service.clone(), service.clone(), String::new()))
        .collect()
}

fn parse_services(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_systemctl_parses_first_column() {
        let services = parse_services(
            "ssh.service loaded active running OpenSSH daemon\ncron.service loaded active running\n",
        );

        assert_eq!(services, vec!["ssh.service", "cron.service"]);
        let suggestions = service_suggestions(&services, "ssh");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "ssh.service");
    }
}
