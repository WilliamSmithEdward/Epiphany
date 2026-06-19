//! Epiphany mdx: a parser and evaluator for the commonly-used MDX set
//! sublanguage, used for dynamic subsets and cellsets.
//!
//! Phase 3 fills this in. The crate is split into a pure, dependency-free front
//! end (this module's [`parse`], over [`lexer`]/[`parser`]) producing a
//! [`SetExpr`] AST, and (from Phase 3B) a tree-walking evaluator over a borrowed
//! dimension/cube. The grammar is documented on [`parser`]. See `docs/ROADMAP.md`.

mod ast;
mod error;
mod eval;
mod evaluator;
mod lexer;
mod parser;

pub use ast::{AxisName, CmpOp, MemberRef, Operand, OrderDir, Predicate, Query, SetExpr};
pub use error::{MdxParseError, ParseErrorKind};
pub use eval::{evaluate, MdxEvalError};
pub use evaluator::MdxEvaluator;
pub use lexer::Span;
pub use parser::{parse, parse_query};

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
