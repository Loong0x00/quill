use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::completion::{
    GenerationId, Provider, ProviderErr, QueryCtx, Suggestion, SuggestionGroup,
};

const CACHE_TTL: Duration = Duration::from_secs(60);

pub struct SshProvider {
    config_path: PathBuf,
    cache: Arc<Mutex<Option<(Vec<String>, Instant)>>>,
}

impl SshProvider {
    pub fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    fn hosts(&self, now: Instant) -> Result<Vec<String>, ProviderErr> {
        if let Some(hosts) = self.cached_hosts(now)? {
            return Ok(hosts);
        }

        let contents = match fs::read_to_string(&self.config_path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(ProviderErr::Io(err.to_string())),
        };
        let hosts = parse_hosts(&contents);

        self.cache
            .lock()
            .map_err(|_| ProviderErr::Io("ssh host cache lock poisoned".to_string()))?
            .replace((hosts.clone(), now));
        Ok(hosts)
    }

    fn cached_hosts(&self, now: Instant) -> Result<Option<Vec<String>>, ProviderErr> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| ProviderErr::Io("ssh host cache lock poisoned".to_string()))?;

        Ok(cache.as_ref().and_then(|(hosts, loaded_at)| {
            (now.duration_since(*loaded_at) < CACHE_TTL).then(|| hosts.clone())
        }))
    }
}

#[async_trait::async_trait]
impl Provider for SshProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if ctx.command != "ssh" {
            return Ok(Vec::new());
        }

        Ok(self
            .hosts(Instant::now())?
            .into_iter()
            .filter(|host| host.starts_with(&ctx.current_token))
            .map(|host| Suggestion {
                text: host.clone(),
                display: host,
                description: String::new(),
                group: SuggestionGroup::Dynamic,
            })
            .collect())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &'static str {
        "ssh"
    }
}

fn parse_hosts(config: &str) -> Vec<String> {
    let mut hosts = Vec::new();

    for line in config.lines() {
        let line = line.split_once('#').map(|(body, _)| body).unwrap_or(line);
        let mut parts = line.split_whitespace();
        if parts
            .next()
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("Host"))
        {
            hosts.extend(
                parts
                    .filter(|host| !is_wildcard_host(host))
                    .map(str::to_string),
            );
        }
    }

    hosts
}

fn is_wildcard_host(host: &str) -> bool {
    host.contains('?') || host.contains('*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_parses_host_lines() {
        let hosts = parse_hosts("Host prod\n    HostName prod.example.com\nHost staging\n");

        assert_eq!(hosts, vec!["prod", "staging"]);
    }

    #[test]
    fn test_ssh_skips_wildcard_and_comments() {
        let hosts = parse_hosts("# Host ignored\nHost *\nHost real\nHost test-* test?\n");

        assert_eq!(hosts, vec!["real"]);
    }

    #[test]
    fn test_ssh_handles_multiple_hosts_per_line() {
        let hosts = parse_hosts("Host alpha beta gamma\n");

        assert_eq!(hosts, vec!["alpha", "beta", "gamma"]);
    }
}
