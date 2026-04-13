//! Candidate filtering — prefix vs fuzzy matching.

use super::model::FilterStrategy;

pub fn matches(strategy: FilterStrategy, candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    // POSIX/GNU short and long flags are case-sensitive: `-v` and `-V` are
    // distinct options. Subcommand and arg-value matching stays
    // case-insensitive (e.g. typing "ch" should still match "Checkout").
    if is_flag(query) {
        return match strategy {
            FilterStrategy::Default | FilterStrategy::Prefix => candidate.starts_with(query),
            FilterStrategy::Fuzzy => candidate.contains(query),
        };
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

fn is_flag(s: &str) -> bool {
    s.starts_with('-')
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

    #[test]
    fn flags_case_sensitive() {
        // Short flags differ by case in POSIX/GNU.
        assert!(matches(FilterStrategy::Prefix, "-l", "-l"));
        assert!(!matches(FilterStrategy::Prefix, "-L", "-l"));
        assert!(!matches(FilterStrategy::Prefix, "-l", "-L"));
        // Long flags are also case-sensitive.
        assert!(matches(FilterStrategy::Prefix, "--long", "--long"));
        assert!(!matches(FilterStrategy::Prefix, "--Long", "--long"));
        // Fuzzy flag matches stay case-sensitive too.
        assert!(matches(FilterStrategy::Fuzzy, "--verbose", "-v"));
        assert!(!matches(FilterStrategy::Fuzzy, "--Verbose", "-v"));
    }
}
