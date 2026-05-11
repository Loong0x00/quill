pub mod cache;
pub mod help_indexer;
pub mod worker;

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

pub use cache::{CacheEntry, CacheKey, CompletionCache};
pub use worker::{WorkItem, WorkerPool};

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Suggestion {
    pub text: String,
    pub display: String,
    pub description: String,
    pub group: SuggestionGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum SuggestionGroup {
    Flag,
    Subcommand,
    File,
    Dynamic,
    History,
}

#[derive(Debug, Clone)]
pub struct QueryCtx {
    pub command: String,
    pub current_token: String,
    pub previous_tokens: Vec<String>,
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErr {
    Timeout,
    Cancelled,
    NotFound,
    Io(String),
    Parse(String),
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn query(
        &self,
        ctx: QueryCtx,
        gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr>;

    fn cancel(&self, gen_id: GenerationId);

    fn name(&self) -> &str;
}

pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.providers.push(provider);
    }

    pub fn query_all(
        &self,
        ctx: QueryCtx,
        gen_id: GenerationId,
        pool: &WorkerPool,
        sender: mpsc::Sender<(GenerationId, Result<Vec<Suggestion>, ProviderErr>)>,
    ) {
        for provider in &self.providers {
            pool.submit(WorkItem {
                provider: Arc::clone(provider),
                ctx: ctx.clone(),
                gen_id,
                result_sender: sender.clone(),
            });
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
