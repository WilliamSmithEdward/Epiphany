//! The rules language: a hand-written, dependency-free lexer, AST, recursive-
//! descent parser, and positioned errors, structured like `epiphany-mdx`.
//!
//! This is the pure front end (Phase 4A). Semantic resolution, compilation to
//! bytecode, and evaluation are separate Phase 4 increments.

mod ast;
mod error;
mod lexer;
mod parser;

pub use ast::{
    Area, ArithOp, BuiltinFunc, CellRef, CmpOp, Condition, DimOverride, DimSelector, Expr, FuncArg,
    FuncCall, Literal, MemberExpr, Rule, RuleDoc, SelectorKind,
};
pub use error::{ParseErrorKind, RuleParseError};
pub use lexer::Span;
pub use parser::parse;
