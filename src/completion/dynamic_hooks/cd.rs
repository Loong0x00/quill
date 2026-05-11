use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, Suggestion};

use super::{file_suggestion, path_prefix, read_dir};

pub struct CdProvider;

#[async_trait::async_trait]
impl Provider for CdProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        if ctx.command != "cd" {
            return Ok(Vec::new());
        }

        let path = path_prefix(&ctx.working_dir, &ctx.current_token);
        let mut suggestions = Vec::new();
        let Some(entries) = read_dir(&path.read_dir)? else {
            return Ok(Vec::new());
        };

        for entry in entries {
            let entry = entry.map_err(|err| ProviderErr::Io(err.to_string()))?;
            let ty = entry
                .file_type()
                .map_err(|err| ProviderErr::Io(err.to_string()))?;
            if !ty.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&path.prefix) {
                continue;
            }

            suggestions.push(file_suggestion(
                format!("{}{name}/", path.text_prefix),
                name,
            ));
        }

        suggestions.sort_by(|left, right| left.display.cmp(&right.display));
        Ok(suggestions)
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "cd"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::dynamic_hooks::{test_query_ctx, test_temp_dir, test_texts};

    #[test]
    fn test_cd_lists_subdirs() {
        let dir = test_temp_dir("cd-lists-subdirs");
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        std::fs::write(dir.join("plain-file"), b"data").unwrap();

        let suggestions = futures::executor::block_on(
            CdProvider.query(test_query_ctx("cd", &dir, ""), GenerationId(1)),
        )
        .unwrap();

        assert_eq!(test_texts(&suggestions), vec!["alpha/", "beta/"]);
        assert!(suggestions
            .iter()
            .all(|suggestion| suggestion.group == crate::completion::SuggestionGroup::File));
    }

    #[test]
    fn test_cd_filters_by_prefix() {
        let dir = test_temp_dir("cd-filters-prefix");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();

        let suggestions = futures::executor::block_on(
            CdProvider.query(test_query_ctx("cd", &dir, "s"), GenerationId(1)),
        )
        .unwrap();

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "src/");
        assert_eq!(suggestions[0].display, "src");
    }
}
