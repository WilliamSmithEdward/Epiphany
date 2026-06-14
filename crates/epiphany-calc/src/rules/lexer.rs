//! Hand-written lexer for the rules language.
//!
//! Produces a flat [`Token`] stream with byte-range [`Span`]s over the source,
//! structured like the MDX lexer. Names and string literals are single-quoted
//! (`'x'`, with `''` for a literal quote); bare words are keywords and function
//! names; `#` starts a line comment and `/* ... */` a block comment. The lexer
//! is pure and zero-dependency.

use crate::rules::error::{ParseErrorKind, RuleParseError};

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
    LBracket,
    RBracket,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Semicolon,
    Colon,
    Bang,
    At,
    Arrow,
    Plus,
    Minus,
    Star,
    Slash,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// A single-quoted name or string literal (contents, unescaped).
    Quoted(String),
    /// A numeric literal, kept as written.
    Number(String),
    /// A bare identifier (keyword or function name).
    Word(String),
}

impl Tok {
    /// A short human-readable rendering, used in parse-error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::LBracket => "[".to_string(),
            Tok::RBracket => "]".to_string(),
            Tok::LParen => "(".to_string(),
            Tok::RParen => ")".to_string(),
            Tok::LBrace => "{".to_string(),
            Tok::RBrace => "}".to_string(),
            Tok::Comma => ",".to_string(),
            Tok::Semicolon => ";".to_string(),
            Tok::Colon => ":".to_string(),
            Tok::Bang => "!".to_string(),
            Tok::At => "@".to_string(),
            Tok::Arrow => "->".to_string(),
            Tok::Plus => "+".to_string(),
            Tok::Minus => "-".to_string(),
            Tok::Star => "*".to_string(),
            Tok::Slash => "/".to_string(),
            Tok::Eq => "=".to_string(),
            Tok::Ne => "<>".to_string(),
            Tok::Lt => "<".to_string(),
            Tok::Le => "<=".to_string(),
            Tok::Gt => ">".to_string(),
            Tok::Ge => ">=".to_string(),
            Tok::Quoted(s) => format!("'{s}'"),
            Tok::Number(n) => n.clone(),
            Tok::Word(w) => w.clone(),
        }
    }
}

/// A token paired with its source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) span: Span,
}

/// Tokenize `src` into a flat token stream (comments and whitespace dropped).
pub(crate) fn lex(src: &str) -> Result<Vec<Token>, RuleParseError> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let n = chars.len();
    let byte_at = |i: usize| if i < n { chars[i].0 } else { src.len() };

    let mut out = Vec::new();
    let mut p = 0;
    while p < n {
        let (start, c) = chars[p];
        if c.is_whitespace() {
            p += 1;
            continue;
        }
        // Comments.
        if c == '#' {
            while p < n && chars[p].1 != '\n' {
                p += 1;
            }
            continue;
        }
        if c == '/' && p + 1 < n && chars[p + 1].1 == '*' {
            let mut q = p + 2;
            loop {
                if q + 1 >= n {
                    return Err(RuleParseError::new(
                        ParseErrorKind::UnterminatedComment,
                        Span::new(start, src.len()),
                    ));
                }
                if chars[q].1 == '*' && chars[q + 1].1 == '/' {
                    q += 2;
                    break;
                }
                q += 1;
            }
            p = q;
            continue;
        }

        let single = |tok: Tok, p: usize| Token {
            tok,
            span: Span::new(start, byte_at(p + 1)),
        };
        match c {
            '[' => {
                out.push(single(Tok::LBracket, p));
                p += 1;
            }
            ']' => {
                out.push(single(Tok::RBracket, p));
                p += 1;
            }
            '(' => {
                out.push(single(Tok::LParen, p));
                p += 1;
            }
            ')' => {
                out.push(single(Tok::RParen, p));
                p += 1;
            }
            '{' => {
                out.push(single(Tok::LBrace, p));
                p += 1;
            }
            '}' => {
                out.push(single(Tok::RBrace, p));
                p += 1;
            }
            ',' => {
                out.push(single(Tok::Comma, p));
                p += 1;
            }
            ';' => {
                out.push(single(Tok::Semicolon, p));
                p += 1;
            }
            ':' => {
                out.push(single(Tok::Colon, p));
                p += 1;
            }
            '!' => {
                out.push(single(Tok::Bang, p));
                p += 1;
            }
            '@' => {
                out.push(single(Tok::At, p));
                p += 1;
            }
            '+' => {
                out.push(single(Tok::Plus, p));
                p += 1;
            }
            '*' => {
                out.push(single(Tok::Star, p));
                p += 1;
            }
            '/' => {
                out.push(single(Tok::Slash, p));
                p += 1;
            }
            '=' => {
                out.push(single(Tok::Eq, p));
                p += 1;
            }
            '-' => {
                if p + 1 < n && chars[p + 1].1 == '>' {
                    out.push(Token {
                        tok: Tok::Arrow,
                        span: Span::new(start, byte_at(p + 2)),
                    });
                    p += 2;
                } else {
                    out.push(single(Tok::Minus, p));
                    p += 1;
                }
            }
            '<' => {
                if p + 1 < n && chars[p + 1].1 == '=' {
                    out.push(Token {
                        tok: Tok::Le,
                        span: Span::new(start, byte_at(p + 2)),
                    });
                    p += 2;
                } else if p + 1 < n && chars[p + 1].1 == '>' {
                    out.push(Token {
                        tok: Tok::Ne,
                        span: Span::new(start, byte_at(p + 2)),
                    });
                    p += 2;
                } else {
                    out.push(single(Tok::Lt, p));
                    p += 1;
                }
            }
            '>' => {
                if p + 1 < n && chars[p + 1].1 == '=' {
                    out.push(Token {
                        tok: Tok::Ge,
                        span: Span::new(start, byte_at(p + 2)),
                    });
                    p += 2;
                } else {
                    out.push(single(Tok::Gt, p));
                    p += 1;
                }
            }
            '\'' => {
                let mut text = String::new();
                let mut q = p + 1;
                loop {
                    if q >= n {
                        return Err(RuleParseError::new(
                            ParseErrorKind::UnterminatedString,
                            Span::new(start, src.len()),
                        ));
                    }
                    let ch = chars[q].1;
                    if ch == '\'' {
                        if q + 1 < n && chars[q + 1].1 == '\'' {
                            text.push('\'');
                            q += 2;
                            continue;
                        }
                        q += 1;
                        break;
                    }
                    text.push(ch);
                    q += 1;
                }
                out.push(Token {
                    tok: Tok::Quoted(text),
                    span: Span::new(start, byte_at(q)),
                });
                p = q;
            }
            _ if c.is_ascii_digit() => {
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
                out.push(Token {
                    tok: Tok::Number(src[start..end].to_string()),
                    span: Span::new(start, end),
                });
                p = q;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let mut q = p + 1;
                while q < n && (chars[q].1.is_alphanumeric() || chars[q].1 == '_') {
                    q += 1;
                }
                let end = byte_at(q);
                out.push(Token {
                    tok: Tok::Word(src[start..end].to_string()),
                    span: Span::new(start, end),
                });
                p = q;
            }
            _ => {
                return Err(RuleParseError::new(
                    ParseErrorKind::UnexpectedChar(c),
                    Span::new(start, byte_at(p + 1)),
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn punctuation_and_operators() {
        assert_eq!(
            toks("[ ] ( ) { } , ; : ! @ -> + - * / = <> < <= > >="),
            vec![
                Tok::LBracket,
                Tok::RBracket,
                Tok::LParen,
                Tok::RParen,
                Tok::LBrace,
                Tok::RBrace,
                Tok::Comma,
                Tok::Semicolon,
                Tok::Colon,
                Tok::Bang,
                Tok::At,
                Tok::Arrow,
                Tok::Plus,
                Tok::Minus,
                Tok::Star,
                Tok::Slash,
                Tok::Eq,
                Tok::Ne,
                Tok::Lt,
                Tok::Le,
                Tok::Gt,
                Tok::Ge,
            ]
        );
    }

    #[test]
    fn quoted_names_with_escape() {
        assert_eq!(toks("'North'"), vec![Tok::Quoted("North".to_string())]);
        assert_eq!(toks("'it''s'"), vec![Tok::Quoted("it's".to_string())]);
    }

    #[test]
    fn numbers_and_words() {
        assert_eq!(
            toks("100 3.14 value IF"),
            vec![
                Tok::Number("100".to_string()),
                Tok::Number("3.14".to_string()),
                Tok::Word("value".to_string()),
                Tok::Word("IF".to_string()),
            ]
        );
    }

    #[test]
    fn comments_are_dropped() {
        assert_eq!(
            toks("'a' # line\n /* block */ 'b'"),
            vec![Tok::Quoted("a".to_string()), Tok::Quoted("b".to_string())]
        );
    }

    #[test]
    fn spans_point_at_tokens_and_line_col() {
        let src = "  'North'";
        let tokens = lex(src).unwrap();
        assert_eq!(tokens[0].span, Span::new(2, 9));
        let src2 = "[\n  'Region'";
        let t = lex(src2).unwrap();
        assert_eq!(t[1].span.line_col(src2), (2, 3));
    }

    #[test]
    fn unterminated_string_and_comment() {
        assert_eq!(
            lex("'oops").unwrap_err().kind,
            ParseErrorKind::UnterminatedString
        );
        assert_eq!(
            lex("/* oops").unwrap_err().kind,
            ParseErrorKind::UnterminatedComment
        );
    }

    #[test]
    fn unexpected_character() {
        let err = lex("a $ b").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::UnexpectedChar('$'));
        assert_eq!(err.span, Span::new(2, 3));
    }
}
