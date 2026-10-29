use std::sync::LazyLock;

use regex::Regex;

use crate::{Catalog, ModelMatch};

static DATE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-(\d{4}-\d{2}-\d{2}|\d{8})$").expect("valid regex"));
static VERSION_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@\d+$").expect("valid regex"));
static CROSS_REGION_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(us|eu|apac|global|jp|au|ca|us-gov)\.").expect("valid regex"));

impl Catalog {
    /// LiteLLM's lookup ladder, simplified: exact, `provider/model`, bare name, stripped variants
    /// (date snapshot, `@version`, Bedrock cross-region prefix, `ft:` fine-tune base), each with and
    /// without the provider prefix, then a case-insensitive pass over the same candidates.
    pub fn resolve(&self, provider: Option<&str>, model: &str) -> Option<ModelMatch<'_>> {
        let candidates = candidates(provider, model);
        candidates
            .iter()
            .find_map(|candidate| self.lookup(candidate))
    }
}

fn candidates(provider: Option<&str>, model: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(12);
    let mut push = |value: String| {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    };
    let bare = model.rsplit('/').next().unwrap_or(model);
    for name in [model, bare] {
        if let Some(provider) = provider {
            push(format!("{provider}/{name}"));
        }
        push(name.to_owned());
    }
    for name in stripped(bare) {
        if let Some(provider) = provider {
            push(format!("{provider}/{name}"));
        }
        push(name.clone());
    }
    out
}

fn stripped(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(rest) = name.strip_prefix("ft:")
        && let Some(base) = rest.split(':').next()
    {
        out.push(base.to_owned());
    }
    let no_region = CROSS_REGION_PREFIX.replace(name, "").into_owned();
    if no_region != name {
        out.push(no_region.clone());
    }
    for base in [name.to_owned(), no_region] {
        let no_date = DATE_SUFFIX.replace(&base, "").into_owned();
        if no_date != base {
            out.push(no_date);
        }
        let no_version = VERSION_SUFFIX.replace(&base, "").into_owned();
        if no_version != base {
            out.push(no_version);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::exact(None, "gpt-4o", "gpt-4o")]
    #[case::prefixed(Some("azure"), "gpt-4o", "azure/gpt-4o")]
    #[case::bare_from_path(Some("openai"), "openai/gpt-4o", "gpt-4o")]
    #[case::date_stripped(Some("openai"), "gpt-4o-2031-01-01", "gpt-4o")]
    #[case::vertex_version(
        Some("vertex_ai"),
        "claude-3-5-sonnet@99999999",
        "vertex_ai/claude-3-5-sonnet"
    )]
    #[case::bedrock_cross_region(
        Some("bedrock"),
        "us.anthropic.claude-3-5-sonnet-20240620-v1:0",
        "anthropic.claude-3-5-sonnet-20240620-v1:0"
    )]
    fn ladder_orders_candidates_from_specific_to_general(
        #[case] provider: Option<&str>,
        #[case] model: &str,
        #[case] expected: &str,
    ) {
        let candidates = candidates(provider, model);
        assert!(
            candidates.iter().any(|candidate| candidate == expected),
            "{candidates:?}"
        );
        let position = |needle: &str| candidates.iter().position(|c| c == needle);
        if let Some(provider) = provider {
            let full = format!("{provider}/{model}");
            assert!(
                position(&full) < position(model),
                "prefixed before exact: {candidates:?}"
            );
        }
    }

    #[rstest]
    fn bundled_catalog_resolves_well_known_models() {
        let catalog = crate::bundled().unwrap();
        assert!(catalog.model_count() > 3000);
        let gpt = catalog.resolve(Some("openai"), "gpt-4o").unwrap();
        assert!(gpt.entry.pricing().has_rates());
        let azure = catalog.resolve(Some("azure"), "gpt-4o").unwrap();
        assert_eq!(azure.canonical_key, "azure/gpt-4o");
        let claude = catalog
            .resolve(Some("anthropic"), "claude-sonnet-4-5-20250929")
            .unwrap();
        assert!(claude.entry.pricing().cache_read.is_some());
        let dated = catalog
            .resolve(Some("anthropic"), "claude-sonnet-4-5-20991231")
            .unwrap();
        assert_eq!(
            dated.canonical_key, "claude-sonnet-4-5",
            "unknown snapshot dates fall back to the base model"
        );
        assert!(
            catalog
                .resolve(Some("openai"), "definitely-not-a-model")
                .is_none()
        );
    }
}
