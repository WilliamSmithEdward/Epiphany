//! Hand-written lexer for the MDX set sublanguage.
//!
//! Produces a flat [`Token`] stream with byte-range [`Span`]s over the source.
//! Bracketed names (`[Region]`) and quoted strings (`"x"` / `'x'`) both support
//! the usual doubled-delimiter escape (`]]` and `""`). Whitespace separates
//! tokens and is otherwise ignored. The lexer is pure and zero-dependency.

use crate::error::{MdxParseError, ParseErrorKind};

/// A half-open byte range `[start, end)` into the parsed source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Span {
    /// Construct a span from byte offsets.
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// The 1-based `(line, column)` of this span's start within `src`, counting
    /// columns in characters. Out-of-range spans clamp to the end of input.
    pub fn line_col(&self, src: &str) -> (usize, usize) {
        let mut line = 1;
        let mut col = 1;
        for (offset, ch) in src.char_indices() {
            if offset >= self.start {
                return (line, col);
            }
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }
}

/// A lexical token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Dot,
    Star,
    /// A name: either bare (`North`) or bracketed (`[North]`). Bracketed names
    /// are never treated as keywords by the parser.
    Name {
        /// The unescaped name text.
        text: String,
        /// Whether the name was written in `[` brackets `]`.
        bracketed: bool,
    },
    /// A quoted string literal (contents, unescaped).
    Str(String),
    /// A numeric literal, kept as written (parsed to `Fixed` at eval time).
    Number(String),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Tok {
    /// A short human-readable rendering, used in parse-error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::LBrace => "{".to_string(),
            Tok::RBrace => "}".to_string(),
            Tok::LParen => "(".to_string(),
            Tok::RParen => ")".to_string(),
            Tok::Comma => ",".to_string(),
            Tok::Dot => ".".to_string(),
            Tok::Star => "*".to_string(),
            Tok::Name {
                text,
                bracketed: true,
            } => format!("[{text}]"),
            Tok::Name {
                text,
                bracketed: false,
            } => text.clone(),
            Tok::Str(s) => format!("\"{s}\""),
            Tok::Number(n) => n.clone(),
            Tok::Eq => "=".to_string(),
            Tok::Ne => "<>".to_string(),
            Tok::Lt => "<".to_string(),
            Tok::Le => "<=".to_string(),
            Tok::Gt => ">".to_string(),
            Tok::Ge => ">=".to_string(),
        }
    }
}

/// A token paired with its source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) span: Span,
}

/// Tokenize `src` into a flat token stream.
pub(crate) fn lex(src: &str) -> Result<Vec<Token>, MdxParseError> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let n = chars.len();
    // Byte offset of char index `i`, or end-of-source for the past-the-end index.
    let byte_at = |i: usize| if i < n { chars[i].0 } else { src.len() };

    let mut out = Vec::new();
    let mut p = 0;
    while p < n {
        let (start, c) = chars[p];
        if c.is_whitespace() {
            p += 1;
            continue;
        }
        match c {
            '{' => {
                out.push(single(Tok::LBrace, start, byte_at(p + 1)));
                p += 1;
            }
            '}' => {
                out.push(single(Tok::RBrace, start, byte_at(p + 1)));
                p += 1;
            }
            '(' => {
                out.push(single(Tok::LParen, start, byte_at(p + 1)));
                p += 1;
            }
            ')' => {
                out.push(single(Tok::RParen, start, byte_at(p + 1)));
                p += 1;
            }
            ',' => {
                out.push(single(Tok::Comma, start, byte_at(p + 1)));
                p += 1;
            }
            '.' => {
                out.push(single(Tok::Dot, start, byte_at(p + 1)));
                p += 1;
            }
            '*' => {
                out.push(single(Tok::Star, start, byte_at(p + 1)));
                p += 1;
            }
            '=' => {
                out.push(single(Tok::Eq, start, byte_at(p + 1)));
                p += 1;
            }
            '<' => {
                if p + 1 < n && chars[p + 1].1 == '=' {
                    out.push(single(Tok::Le, start, byte_at(p + 2)));
                    p += 2;
                } else if p + 1 < n && chars[p + 1].1 == '>' {
                    out.push(single(Tok::Ne, start, byte_at(p + 2)));
                    p += 2;
                } else {
                    out.push(single(Tok::Lt, start, byte_at(p + 1)));
                    p += 1;
                }
            }
            '>' => {
                if p + 1 < n && chars[p + 1].1 == '=' {
                    out.push(single(Tok::Ge, start, byte_at(p + 2)));
                    p += 2;
                } else {
                    out.push(single(Tok::Gt, start, byte_at(p + 1)));
                    p += 1;
                }
            }
            '[' => {
                let mut text = String::new();
                let mut q = p + 1;
                loop {
                    if q >= n {
                        return Err(MdxParseError::new(
                            ParseErrorKind::UnterminatedBracket,
                            Span::new(start, src.len()),
                        ));
                    }
                    let ch = chars[q].1;
                    if ch == ']' {
                        if q + 1 < n && chars[q + 1].1 == ']' {
                            text.push(']');
                            q += 2;
                            continue;
                        }
                        q += 1;
                        break;
                    }
                    text.push(ch);
                    q += 1;
                }
                out.push(single(
                    Tok::Name {
                        text,
                        bracketed: true,
                    },
                    start,
                    byte_at(q),
                ));
                p = q;
            }
            '"' | '\'' => {
                let quote = c;
                let mut text = String::new();
                let mut q = p + 1;
                loop {
                    if q >= n {
                        return Err(MdxParseError::new(
                            ParseErrorKind::UnterminatedString,
                            Span::new(start, src.len()),
                        ));
                    }
                    let ch = chars[q].1;
                    if ch == quote {
                        if q + 1 < n && chars[q + 1].1 == quote {
                            text.push(quote);
                            q += 2;
                            continue;
                        }
                        q += 1;
                        break;
                    }
                    text.push(ch);
                    q += 1;
                }
                out.push(single(Tok::Str(text), start, byte_at(q)));
                p = q;
            }
            _ if c.is_ascii_digit()
                || (c == '-' && p + 1 < n && chars[p + 1].1.is_ascii_digit()) =>
            {
                let mut q = p + 1;
                while q < n && chars[q].1.is_ascii_digit() {
                    q += 1;
                }
                if q < n && chars[q].1 == '.' {
                    q += 1;
                    while q < n && chars[q].1.is_ascii_digit() {
                        q += 1;
                    }
                }
                let end = byte_at(q);
                out.push(single(Tok::Number(src[start..end].to_string()), start, end));
                p = q;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let mut q = p + 1;
                while q < n && (chars[q].1.is_alphanumeric() || chars[q].1 == '_') {
                    q += 1;
                }
                let end = byte_at(q);
                out.push(single(
                    Tok::Name {
                        text: src[start..end].to_string(),
                        bracketed: false,
                    },
                    start,
                    end,
                ));
                p = q;
            }
            _ => {
                return Err(MdxParseError::new(
                    ParseErrorKind::UnexpectedChar(c),
                    Span::new(start, byte_at(p + 1)),
                ));
            }
        }
    }
    Ok(out)
}

fn single(tok: Tok, start: usize, end: usize) -> Token {
    Token {
        tok,
        span: Span::new(start, end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_structural_punctuation() {
        assert_eq!(
            toks("{ } ( ) , . *"),
            vec![
                Tok::LBrace,
                Tok::RBrace,
                Tok::LParen,
                Tok::RParen,
                Tok::Comma,
                Tok::Dot,
                Tok::Star,
            ]
        );
    }

    #[test]
    fn lexes_comparison_operators() {
        assert_eq!(
            toks("= <> < <= > >="),
            vec![Tok::Eq, Tok::Ne, Tok::Lt, Tok::Le, Tok::Gt, Tok::Ge]
        );
    }

    #[test]
    fn bare_and_bracketed_names() {
        assert_eq!(
            toks("[Region].North"),
            vec![
                Tok::Name {
                    text: "Region".to_string(),
                    bracketed: true
                },
                Tok::Dot,
                Tok::Name {
                    text: "North".to_string(),
                    bracketed: false
                },
            ]
        );
    }

    #[test]
    fn bracket_and_quote_escapes() {
        assert_eq!(
            toks("[a]]b]"),
            vec![Tok::Name {
                text: "a]b".to_string(),
                bracketed: true
            }]
        );
        assert_eq!(
            toks("\"he said \"\"hi\"\"\""),
            vec![Tok::Str("he said \"hi\"".to_string())]
        );
        assert_eq!(toks("'it''s'"), vec![Tok::Str("it's".to_string())]);
    }

    #[test]
    fn numbers_including_negative_and_decimal() {
        assert_eq!(
            toks("100 -5 3.14"),
            vec![
                Tok::Number("100".to_string()),
                Tok::Number("-5".to_string()),
                Tok::Number("3.14".to_string()),
            ]
        );
    }

    #[test]
    fn spans_point_at_the_token() {
        let tokens = lex("  [North]").unwrap();
        assert_eq!(tokens[0].span, Span::new(2, 9));
    }

    #[test]
    fn unterminated_bracket_spans_to_end() {
        let err = lex("[North").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::UnterminatedBracket);
        assert_eq!(err.span, Span::new(0, 6));
    }

    #[test]
    fn unterminated_string_spans_to_end() {
        let err = lex("\"oops").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::UnterminatedString);
        assert_eq!(err.span, Span::new(0, 5));
    }

    #[test]
    fn unexpected_character_is_reported() {
        let err = lex("a # b").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::UnexpectedChar('#'));
        assert_eq!(err.span, Span::new(2, 3));
    }

    #[test]
    fn line_col_is_one_based() {
        let src = "{\n  [North]";
        let span = lex(src).unwrap()[1].span;
        assert_eq!(span.line_col(src), (2, 3));
    }
}
