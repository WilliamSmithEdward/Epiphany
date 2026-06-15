//! Positioned parse errors for the MDX set sublanguage.

use std::fmt;

use crate::lexer::Span;

/// A parse failure together with the byte span it occurred at.
///
/// The span indexes the original source string and can be turned into a 1-based
/// line/column via [`Span::line_col`] for API error details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdxParseError {
    /// What went wrong.
    pub kind: ParseErrorKind,
    /// Where in the source it went wrong.
    pub span: Span,
}

impl MdxParseError {
    pub(crate) fn new(kind: ParseErrorKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// The category of a lex or parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// A token was found where a different construct was expected.
    UnexpectedToken {
        /// A human-readable rendering of the offending token.
        found: String,
        /// What the parser was looking for instead.
        expected: &'static str,
    },
    /// The input ended while more was expected.
    UnexpectedEof {
        /// What the parser was looking for.
        expected: &'static str,
    },
    /// A `[`-bracketed name was never closed.
    UnterminatedBracket,
    /// A quoted string was never closed.
    UnterminatedString,
    /// A character that cannot begin any token.
    UnexpectedChar(char),
    /// Tokens remained after an otherwise complete set expression.
    TrailingInput,
    /// The expression nests deeper than the parser allows (a stack-exhaustion
    /// guard, not a real limit on any hand-authored query).
    TooDeep,
}

impl fmt::Display for MdxParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseErrorKind::UnexpectedToken { found, expected } => {
                write!(f, "unexpected `{found}`, expected {expected}")
            }
            ParseErrorKind::UnexpectedEof { expected } => {
                write!(f, "unexpected end of input, expected {expected}")
            }
            ParseErrorKind::UnterminatedBracket => write!(f, "unterminated `[` name"),
            ParseErrorKind::UnterminatedString => write!(f, "unterminated string"),
            ParseErrorKind::UnexpectedChar(c) => write!(f, "unexpected character `{c}`"),
            ParseErrorKind::TrailingInput => write!(f, "unexpected trailing input"),
            ParseErrorKind::TooDeep => write!(f, "expression nests too deeply"),
        }
    }
}

impl std::error::Error for MdxParseError {}
