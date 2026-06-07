//! Candidate filtering — prefix vs fuzzy matching.

use super::model::FilterStrategy;

pub fn matches(strategy: FilterStrategy, candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    // Upstream matches ALL suggestion names case-insensitively, flags
    // included: typing `-c` surfaces both `-C` and `-c` as distinct
    // options (verified against the installed upstream binary). A prior
    // change here special-cased flags as case-sensitive; that diverged
    // from upstream and has been reverted.
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

    #[test]
    fn flags_case_insensitive() {
        // Upstream matches flags case-insensitively: typing `-c` surfaces
        // both `-C` and `-c` as candidates, so both directions match.
        assert!(matches(FilterStrategy::Prefix, "-l", "-l"));
        assert!(matches(FilterStrategy::Prefix, "-L", "-l"));
        assert!(matches(FilterStrategy::Prefix, "-l", "-L"));
        // Long flags too.
        assert!(matches(FilterStrategy::Prefix, "--long", "--long"));
        assert!(matches(FilterStrategy::Prefix, "--Long", "--long"));
        // Fuzzy flag matches are case-insensitive as well.
        assert!(matches(FilterStrategy::Fuzzy, "--verbose", "-v"));
        assert!(matches(FilterStrategy::Fuzzy, "--Verbose", "-v"));
    }
}
