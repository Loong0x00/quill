use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{
    cached_values, ctx_tokens, dynamic_suggestion, external_cache_signature, matching_cache_values,
    spawn_external_refresh,
};

pub struct KillProvider {
    cache: Arc<Mutex<Option<(Vec<String>, Instant)>>>,
    ttl: Duration,
}

impl KillProvider {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            ttl: Duration::from_secs(2),
        }
    }
}

impl Default for KillProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for KillProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !ctx_tokens(&ctx)
            .first()
            .is_some_and(|token| token == "kill")
        {
            return Ok(Vec::new());
        }

        let signature = external_cache_signature(&ctx, "kill", "processes");
        if let Some(values) = cached_values(&self.cache, self.ttl, Instant::now())? {
            if let Some(processes) = matching_cache_values(values, &signature) {
                return Ok(process_suggestions(&processes, &ctx.current_token));
            }
        }

        spawn_external_refresh(
            "quill-kill-hook",
            Arc::clone(&self.cache),
            ctx.working_dir,
            "ps",
            vec![
                "-eo".to_string(),
                "pid,comm".to_string(),
                "--no-headers".to_string(),
            ],
            signature,
            parse_processes,
        )?;
        Ok(Vec::new())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "kill"
    }
}

fn process_suggestions(processes: &[String], prefix: &str) -> Vec<Suggestion> {
    processes
        .iter()
        .filter_map(|process| {
            let (pid, comm) = process.split_once('\t')?;
            pid.starts_with(prefix).then(|| {
                dynamic_suggestion(pid.to_string(), format!("{pid}({comm})"), comm.to_string())
            })
        })
        .collect()
}

fn parse_processes(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?;
            let comm = parts.next().unwrap_or_default();
            Some(format!("{pid}\t{comm}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kill_parses_processes_and_formats_display() {
        let processes = parse_processes(" 123 zsh\n456 rust-analyzer\n");

        assert_eq!(processes, vec!["123\tzsh", "456\trust-analyzer"]);
        let suggestions = process_suggestions(&processes, "12");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "123");
        assert_eq!(suggestions[0].display, "123(zsh)");
    }
}
