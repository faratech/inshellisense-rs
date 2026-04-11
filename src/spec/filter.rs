//! Candidate filtering — prefix vs fuzzy matching.

use super::model::FilterStrategy;

pub fn matches(strategy: FilterStrategy, candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    match strategy {
        FilterStrategy::Default | FilterStrategy::Prefix => {
            ci_starts_with(candidate, query)
        }
        FilterStrategy::Fuzzy => {
            // Substring match — matches inshellisense's default fuzzy which
            // is really case-insensitive contains.
            candidate.to_lowercase().contains(&query.to_lowercase())
        }
    }
}

fn ci_starts_with(s: &str, prefix: &str) -> bool {
    let sl = s.chars().flat_map(|c| c.to_lowercase()).collect::<String>();
    let pl = prefix.chars().flat_map(|c| c.to_lowercase()).collect::<String>();
    sl.starts_with(&pl)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_case_insensitive() {
        assert!(matches(FilterStrategy::Prefix, "Checkout", "ch"));
        assert!(matches(FilterStrategy::Prefix, "checkout", "Ch"));
        assert!(!matches(FilterStrategy::Prefix, "commit", "ch"));
    }

    #[test]
    fn fuzzy_contains() {
        assert!(matches(FilterStrategy::Fuzzy, "git-checkout", "check"));
        assert!(!matches(FilterStrategy::Fuzzy, "git-commit", "check"));
    }
}
