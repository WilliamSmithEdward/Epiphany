//! Splits query text into tokens.

use super::MdxError;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
    Number(f64),
    /// A quoted string.
    Text(String),
    /// A name in square brackets.
    Name(String),
    /// A bare word: a keyword, a function name or an unbracketed name.
    Word(String),
    Symbol(&'static str),
    End,
}

/// Longer symbols first, so that `<=` is not read as `<` then `=`.
const SYMBOLS: [&str; 17] = [
    "<>", "<=", ">=", "(", ")", "{", "}", ",", ".", "+", "-", "*", "/", "=", "<", ">", ":",
];

/// The tokens of a query, each with the position of its first character.
pub(super) fn lex(source: &str) -> Result<Vec<(Token, usize)>, MdxError> {
    let characters: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    let error = |message: &str, position| MdxError {
        message: message.to_owned(),
        position: Some(position),
    };
    // Reads up to an unescaped closing character; doubling it stands for the character itself.
    let quoted = |start: usize, close: char| {
        let mut text = String::new();
        let mut at = start;
        loop {
            match (characters.get(at), characters.get(at + 1)) {
                (Some(&c), Some(&next)) if c == close && next == close => {
                    text.push(close);
                    at += 2;
                }
                (Some(&c), _) if c == close => return Some((text, at + 1)),
                (Some(&c), _) => {
                    text.push(c);
                    at += 1;
                }
                (None, _) => return None,
            }
        }
    };
    let run = |start: usize, keep: fn(char) -> bool| {
        let length = characters[start..].iter().take_while(|&&c| keep(c)).count();
        (
            characters[start..start + length].iter().collect::<String>(),
            start + length,
        )
    };

    while let Some(&c) = characters.get(at) {
        let start = at;
        if c.is_whitespace() {
            at += 1;
            continue;
        }
        if c == '-' && characters.get(at + 1) == Some(&'-') {
            at += characters[at..].iter().take_while(|&&c| c != '\n').count();
            continue;
        }
        let token = if c == '[' {
            let (name, end) = quoted(at + 1, ']').ok_or_else(|| error("unclosed '['", start))?;
            at = end;
            Token::Name(name)
        } else if c == '"' || c == '\'' {
            let (text, end) = quoted(at + 1, c).ok_or_else(|| error("unclosed quote", start))?;
            at = end;
            Token::Text(text)
        } else if c.is_ascii_digit() {
            let (digits, end) = run(at, |c| c.is_ascii_digit() || c == '.');
            at = end;
            let number = digits
                .parse()
                .map_err(|_| error("malformed number", start))?;
            Token::Number(number)
        } else if c.is_alphabetic() || c == '_' {
            let (word, end) = run(at, |c| c.is_alphanumeric() || c == '_');
            at = end;
            Token::Word(word)
        } else {
            let rest: String = characters[at..].iter().take(2).collect();
            let symbol = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol));
            let symbol = *symbol.ok_or_else(|| error(&format!("unexpected '{c}'"), start))?;
            at += symbol.len();
            Token::Symbol(symbol)
        };
        tokens.push((token, start));
    }
    tokens.push((Token::End, characters.len()));
    Ok(tokens)
}
