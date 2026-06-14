//! Positioned parse errors for the rules language.

use std::fmt;

use crate::rules::lexer::Span;

/// A parse failure together with the byte span it occurred at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleParseError {
    /// What went wrong.
    pub kind: ParseErrorKind,
    /// Where in the source it went wrong.
    pub span: Span,
}

impl RuleParseError {
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
    /// A single-quoted name or string was never closed.
    UnterminatedString,
    /// A `/* ... */` block comment was never closed.
    UnterminatedComment,
    /// A character that cannot begin any token.
    UnexpectedChar(char),
    /// Tokens remained after an otherwise complete rule document.
    TrailingInput,
    /// An area named the same dimension more than once.
    DuplicateDimension(String),
    /// A function call named a word that is not in the allow-list.
    UnknownFunction(String),
}

impl fmt::Display for RuleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseErrorKind::UnexpectedToken { found, expected } => {
                write!(f, "unexpected `{found}`, expected {expected}")
            }
            ParseErrorKind::UnexpectedEof { expected } => {
                write!(f, "unexpected end of input, expected {expected}")
            }
            ParseErrorKind::UnterminatedString => write!(f, "unterminated quoted name or string"),
            ParseErrorKind::UnterminatedComment => write!(f, "unterminated block comment"),
            ParseErrorKind::UnexpectedChar(c) => write!(f, "unexpected character `{c}`"),
            ParseErrorKind::TrailingInput => write!(f, "unexpected trailing input"),
            ParseErrorKind::DuplicateDimension(d) => {
                write!(f, "dimension '{d}' appears more than once in the area")
            }
            ParseErrorKind::UnknownFunction(name) => write!(f, "unknown function '{name}'"),
        }
    }
}

impl std::error::Error for RuleParseError {}
