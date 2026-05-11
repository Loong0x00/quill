use std::collections::HashSet;

use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{file_suggestion, path_prefix, read_dir};

pub struct ReaddirProvider {
    target_commands: HashSet<String>,
}

impl ReaddirProvider {
    pub fn new_default() -> Self {
        let target_commands = [
            "ls", "cat", "bat", "less", "vim", "nvim", "nano", "rm", "cp", "mv",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        Self { target_commands }
    }
}

#[async_trait::async_trait]
impl Provider for ReaddirProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if !self.target_commands.contains(&ctx.command) || ctx.current_token.starts_with('-') {
            return Ok(Vec::new());
        }

        let path = path_prefix(&ctx.working_dir, &ctx.current_token);
        let Some(entries) = read_dir(&path.read_dir)? else {
            return Ok(Vec::new());
        };

        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| ProviderErr::Io(err.to_string()))?;
            let ty = entry
                .file_type()
                .map_err(|err| ProviderErr::Io(err.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&path.prefix) {
                continue;
            }
            if name.starts_with('.') && !path.prefix.starts_with('.') {
                continue;
            }

            candidates.push((name, ty.is_dir()));
        }

        candidates.sort_by(|(left_name, left_dir), (right_name, right_dir)| {
            right_dir
                .cmp(left_dir)
                .then_with(|| left_name.cmp(right_name))
        });

        Ok(candidates
            .into_iter()
            .map(|(name, is_dir)| {
                let suffix = if is_dir { "/" } else { "" };
                file_suggestion(
                    format!("{}{}{}", path.text_prefix, name, suffix),
                    format!("{name}{suffix}"),
                )
            })
            .collect())
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "readdir"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::dynamic_hooks::{test_query_ctx, test_temp_dir, test_texts};

    #[test]
    fn test_readdir_lists_files_and_dirs() {
        let dir = test_temp_dir("readdir-lists-files-and-dirs");
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        std::fs::write(dir.join("note.txt"), b"data").unwrap();
        let provider = ReaddirProvider::new_default();

        let suggestions = futures::executor::block_on(
            provider.query(test_query_ctx("ls", &dir, ""), GenerationId(1)),
        )
        .unwrap();

        assert_eq!(test_texts(&suggestions), vec!["folder/", "note.txt"]);
    }

    #[test]
    fn test_readdir_hides_dotfiles_unless_prefix_dot() {
        let dir = test_temp_dir("readdir-dotfiles");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), b"secret").unwrap();
        std::fs::write(dir.join("visible"), b"data").unwrap();
        let provider = ReaddirProvider::new_default();

        let visible = futures::executor::block_on(
            provider.query(test_query_ctx("cat", &dir, ""), GenerationId(1)),
        )
        .unwrap();
        assert!(!visible.iter().any(|suggestion| suggestion.text == ".env"));

        let hidden = futures::executor::block_on(
            provider.query(test_query_ctx("cat", &dir, "."), GenerationId(2)),
        )
        .unwrap();
        assert!(hidden.iter().any(|suggestion| suggestion.text == ".env"));
    }

    #[test]
    fn test_readdir_handles_relative_path() {
        let dir = test_temp_dir("readdir-relative-path");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested").join("alpha.txt"), b"data").unwrap();
        std::fs::write(dir.join("nested").join("beta.txt"), b"data").unwrap();
        let provider = ReaddirProvider::new_default();

        let suggestions = futures::executor::block_on(
            provider.query(test_query_ctx("vim", &dir, "nested/a"), GenerationId(1)),
        )
        .unwrap();

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "nested/alpha.txt");
        assert_eq!(suggestions[0].display, "alpha.txt");
    }
}
