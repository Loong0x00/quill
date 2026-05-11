use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::completion::{GenerationId, Provider, ProviderResult, QueryCtx};

pub struct WorkerPool {
    sender: mpsc::Sender<WorkItem>,
    pub(crate) inflight: Arc<Mutex<HashMap<GenerationId, ()>>>,
    counts: Arc<Mutex<HashMap<GenerationId, usize>>>,
    providers: Arc<Mutex<HashMap<GenerationId, Vec<Arc<dyn Provider>>>>>,
}

pub struct WorkItem {
    pub provider: Arc<dyn Provider>,
    pub ctx: QueryCtx,
    pub gen_id: GenerationId,
    pub result_sender: mpsc::Sender<ProviderResult>,
}

impl WorkerPool {
    pub fn new(num_workers: usize) -> Self {
        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));
        let inflight = Arc::new(Mutex::new(HashMap::new()));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let providers = Arc::new(Mutex::new(HashMap::new()));
        let worker_count = num_workers.max(1);

        for worker_idx in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let inflight = Arc::clone(&inflight);
            let counts = Arc::clone(&counts);
            let providers = Arc::clone(&providers);
            let builder = thread::Builder::new().name(format!("quill-completion-{worker_idx}"));

            let _ = builder.spawn(move || loop {
                let item = match receiver.lock() {
                    Ok(receiver) => receiver.recv(),
                    Err(_) => return,
                };

                match item {
                    Ok(item) => run_item(item, &inflight, &counts, &providers),
                    Err(_) => return,
                }
            });
        }

        Self {
            sender,
            inflight,
            counts,
            providers,
        }
    }

    pub fn submit(&self, item: WorkItem) {
        let gen_id = item.gen_id;
        let provider = Arc::clone(&item.provider);

        if let Ok(mut inflight) = self.inflight.lock() {
            inflight.insert(gen_id, ());
        }
        if let Ok(mut counts) = self.counts.lock() {
            *counts.entry(gen_id).or_insert(0) += 1;
        }
        if let Ok(mut providers) = self.providers.lock() {
            providers.entry(gen_id).or_default().push(provider);
        }

        if self.sender.send(item).is_err() {
            self.finish_generation(gen_id);
        }
    }

    pub fn cancel(&self, gen_id: GenerationId) {
        if let Ok(mut inflight) = self.inflight.lock() {
            inflight.remove(&gen_id);
        }
        if let Ok(mut counts) = self.counts.lock() {
            counts.remove(&gen_id);
        }

        let providers = match self.providers.lock() {
            Ok(mut providers) => providers.remove(&gen_id).unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        for provider in providers {
            provider.cancel(gen_id);
        }
    }

    fn finish_generation(&self, gen_id: GenerationId) {
        finish_generation(gen_id, &self.inflight, &self.counts, &self.providers);
    }
}

fn run_item(
    item: WorkItem,
    inflight: &Arc<Mutex<HashMap<GenerationId, ()>>>,
    counts: &Arc<Mutex<HashMap<GenerationId, usize>>>,
    providers: &Arc<Mutex<HashMap<GenerationId, Vec<Arc<dyn Provider>>>>>,
) {
    if !is_active(item.gen_id, inflight) {
        finish_generation(item.gen_id, inflight, counts, providers);
        return;
    }

    let gen_id = item.gen_id;
    let provider_name = item.provider.name();
    let result = futures::executor::block_on(item.provider.query(item.ctx, gen_id));
    if is_active(gen_id, inflight) {
        match result {
            Ok(suggestions) => {
                let _ = item
                    .result_sender
                    .send((gen_id, suggestions, provider_name));
            }
            Err(err) => {
                tracing::trace!(
                    target: "quill::completion",
                    provider = provider_name,
                    ?err,
                    "completion provider returned no suggestions"
                );
                let _ = item.result_sender.send((gen_id, Vec::new(), provider_name));
            }
        }
    }
    finish_generation(gen_id, inflight, counts, providers);
}

fn is_active(gen_id: GenerationId, inflight: &Arc<Mutex<HashMap<GenerationId, ()>>>) -> bool {
    inflight
        .lock()
        .map(|inflight| inflight.contains_key(&gen_id))
        .unwrap_or(false)
}

fn finish_generation(
    gen_id: GenerationId,
    inflight: &Arc<Mutex<HashMap<GenerationId, ()>>>,
    counts: &Arc<Mutex<HashMap<GenerationId, usize>>>,
    providers: &Arc<Mutex<HashMap<GenerationId, Vec<Arc<dyn Provider>>>>>,
) {
    let remove_generation = match counts.lock() {
        Ok(mut counts) => match counts.get_mut(&gen_id) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => {
                counts.remove(&gen_id);
                true
            }
            None => true,
        },
        Err(_) => true,
    };

    if remove_generation {
        if let Ok(mut inflight) = inflight.lock() {
            inflight.remove(&gen_id);
        }
        if let Ok(mut providers) = providers.lock() {
            providers.remove(&gen_id);
        }
    }
}
