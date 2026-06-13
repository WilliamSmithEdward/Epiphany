//! Epiphany mdx — MDX parser/evaluator for dynamic subsets and cellsets.
//!
//! Phase 0 skeleton. Phase 3 fills this in (the commonly-used MDX subset:
//! membership, level/attribute filtering, sorting). See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-mdx";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-mdx");
    }

    #[test]
    fn links_core() {
        assert_eq!(epiphany_core::CRATE, "epiphany-core");
    }
}
