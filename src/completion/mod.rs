pub mod bootstrap;
pub mod cache;
pub mod dynamic_hooks;
pub mod help_indexer;
pub mod parser;
pub mod worker;

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, OnceLock};

pub use cache::{CacheEntry, CacheKey, CompletionCache};
use dynamic_hooks::{
    CdProvider, DockerProvider, GitBranchProvider, GitStatusProvider, KillProvider,
    KubectlProvider, PacmanProvider, ReaddirProvider, SshProvider, SystemctlProvider,
};
use help_indexer::{HelpIndexerConfig, HelpIndexerProvider};
pub use worker::{WorkItem, WorkerPool};

pub type ProviderResult = (GenerationId, Vec<Suggestion>, &'static str);

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

    fn name(&self) -> &'static str;
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

    pub fn new_default() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(HelpIndexerProvider::new(
            shared_completion_cache(),
            HelpIndexerConfig::default(),
        )));
        registry.register(Arc::new(CdProvider));
        registry.register(Arc::new(SshProvider::new(
            dynamic_hooks::default_ssh_config_path(),
        )));
        registry.register(Arc::new(ReaddirProvider::new_default()));
        registry.register(Arc::new(GitBranchProvider::new()));
        registry.register(Arc::new(GitStatusProvider::new()));
        registry.register(Arc::new(KillProvider::new()));
        registry.register(Arc::new(DockerProvider::new()));
        registry.register(Arc::new(KubectlProvider::new()));
        registry.register(Arc::new(PacmanProvider::new()));
        registry.register(Arc::new(SystemctlProvider::new()));
        registry
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.providers.push(provider);
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub fn query_all(
        &self,
        ctx: QueryCtx,
        gen_id: GenerationId,
        pool: &WorkerPool,
        sender: mpsc::Sender<ProviderResult>,
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
        Self::new_default()
    }
}

pub fn shared_completion_cache() -> Arc<Mutex<CompletionCache>> {
    static CACHE: OnceLock<Arc<Mutex<CompletionCache>>> = OnceLock::new();
    Arc::clone(CACHE.get_or_init(|| Arc::new(Mutex::new(CompletionCache::default()))))
}
