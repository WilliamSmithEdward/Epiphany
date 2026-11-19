//! Builds the syntax tree of a query from its tokens.

use super::MdxError;
use super::lexer::{Token, lex};
use crate::model::ElementId;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Number(f64),
    Text(String),
    /// A dotted name such as `[Account].[Revenue]`, or a single word such as `DESC`.
    Name(Vec<String>),
    /// A name already found to be an element of the dimension at a position in the cube.
    Member(usize, ElementId),
    /// A name already found to be the dimension at a position in the cube.
    Dimension(usize),
    /// `{ a, b, ... }`
    Set(Vec<Expr>),
    /// `( a, b, ... )` with more than one item.
    Tuple(Vec<Expr>),
    /// A function call; the name is in upper case.
    Call(String, Vec<Expr>),
    /// `target.NAME` or `target.NAME(arguments)`; the name is in upper case.
    Method(Box<Expr>, String, Vec<Expr>),
    Not(Box<Expr>),
    Negate(Box<Expr>),
    Binary(Box<Expr>, &'static str, Box<Expr>),
}

#[derive(Debug)]
pub(crate) struct Query {
    /// `WITH MEMBER name AS expression`, in order.
    pub(super) members: Vec<MemberDef>,
    /// `WITH SET name AS expression`, in order.
    pub(super) sets: Vec<(String, Expr)>,
    pub(super) axes: Vec<Axis>,
    pub(crate) cube: String,
    pub(super) slicer: Option<Expr>,
}

#[derive(Debug)]
pub(super) struct MemberDef {
    pub(super) name: Vec<String>,
    pub(super) expression: Expr,
    /// Where a cell has several calculated members, the highest is calculated last, over
    /// the results of the others.
    pub(super) solve_order: f64,
}

#[derive(Debug)]
pub(super) struct Axis {
    pub(super) non_empty: bool,
    pub(super) set: Expr,
    /// The attributes to report for each member, as dotted names.
    pub(super) properties: Vec<Vec<String>>,
    /// Zero for columns, one for rows, and so on.
    pub(super) ordinal: usize,
}

/// The names of the first five axes.
const AXES: [&str; 5] = ["COLUMNS", "ROWS", "PAGES", "CHAPTERS", "SECTIONS"];

/// Binary operators from loosest to tightest. `NOT` sits between `AND` and the comparisons.
const LEVELS: [&[&str]; 5] = [
    &["OR"],
    &["AND"],
    &["=", "<>", "<=", ">=", "<", ">"],
    &["+", "-"],
    &["*", "/"],
];
const COMPARISONS: usize = 2;

/// The words that may follow a dot without being part of a name.
const METHODS: [&str; 18] = [
    "PREVMEMBER",
    "NEXTMEMBER",
    "LAG",
    "LEAD",
    "ALLMEMBERS",
    "SIBLINGS",
    "COUNT",
    "ITEM",
    "MEMBERS",
    "CHILDREN",
    "PARENT",
    "CURRENTMEMBER",
    "NAME",
    "UNIQUENAME",
    "VALUE",
    "PROPERTIES",
    "FIRSTCHILD",
    "LASTCHILD",
];

pub(super) struct Parser {
    tokens: Vec<(Token, usize)>,
    next: usize,
}

type Parsed<T> = Result<T, MdxError>;

impl Parser {
    pub(super) fn new(tokens: Vec<(Token, usize)>) -> Self {
        Self { tokens, next: 0 }
    }

    pub(super) fn query(mut self) -> Parsed<Query> {
        let (mut members, mut sets) = (Vec::new(), Vec::new());
        if self.word("WITH") {
            loop {
                if self.word("MEMBER") {
                    let Expr::Name(name) = self.name()? else {
                        unreachable!("a name is a name");
                    };
                    self.expect_word("AS")?;
                    let expression = self.definition()?;
                    // Properties follow as `, NAME = value`. Only the solve order matters
                    // here; the others, such as a format, are accepted and ignored.
                    let mut solve_order = 0.0;
                    while self.symbol(",") {
                        let Token::Word(property) = self.advance() else {
                            return Err(self.error("expected a property name"));
                        };
                        self.expect_symbol("=")?;
                        let value = self.unary()?;
                        if property.eq_ignore_ascii_case("SOLVE_ORDER") {
                            solve_order = match value {
                                Expr::Number(number) => number,
                                _ => return Err(self.error("SOLVE_ORDER takes a number")),
                            };
                        }
                    }
                    members.push(MemberDef {
                        name,
                        expression,
                        solve_order,
                    });
                } else if self.word("SET") {
                    let Expr::Name(name) = self.name()? else {
                        unreachable!("a name is a name");
                    };
                    let [name] = <[String; 1]>::try_from(name)
                        .map_err(|_| self.error("a set name has one part"))?;
                    self.expect_word("AS")?;
                    sets.push((name, self.definition()?));
                } else {
                    break;
                }
            }
        }

        self.expect_word("SELECT")?;
        let mut axes = Vec::new();
        while !self.word("FROM") {
            if !axes.is_empty() {
                self.expect_symbol(",")?;
            }
            axes.push(self.axis()?);
        }
        let cube = match self.advance() {
            Token::Name(cube) | Token::Word(cube) => cube,
            _ => return Err(self.error("expected a cube name")),
        };
        let slicer = match self.word("WHERE") {
            true => Some(self.expression()?),
            false => None,
        };
        self.expect_end()?;
        Ok(Query {
            members,
            sets,
            axes,
            cube,
            slicer,
        })
    }

    /// A rule: `[N: | C: | S:] area = expression [;]`. Returns the scope letter, the area and
    /// the expression.
    pub(super) fn rule(mut self) -> Parsed<(Option<char>, Expr, Expr)> {
        let scope = match (self.peek(), self.after()) {
            (Token::Word(word), Token::Symbol(":"))
                if ["N", "C", "S"].contains(&&*word.to_uppercase()) =>
            {
                word.to_uppercase().chars().next()
            }
            _ => None,
        };
        self.next += 2 * usize::from(scope.is_some());
        let area = self.primary()?;
        self.expect_symbol("=")?;
        let expression = self.expression()?;
        self.symbol(";");
        self.expect_end()?;
        Ok((scope, area, expression))
    }

    /// A feeder: `source => target [, target ...] [;]`.
    pub(super) fn feeder(mut self) -> Parsed<(Expr, Vec<Expr>)> {
        let source = self.primary()?;
        self.expect_symbol("=>")?;
        let mut targets = vec![self.expression()?];
        while self.symbol(",") {
            targets.push(self.expression()?);
        }
        self.symbol(";");
        self.expect_end()?;
        Ok((source, targets))
    }

    /// A set expression on its own, as a subset is written.
    pub(super) fn set(mut self) -> Parsed<Expr> {
        let expression = self.expression()?;
        self.expect_end()?;
        Ok(expression)
    }

    /// The expression after `AS`, which older queries wrap in quotes.
    fn definition(&mut self) -> Parsed<Expr> {
        let Token::Text(text) = self.peek().clone() else {
            return self.expression();
        };
        self.advance();
        let mut inner = Parser::new(lex(&text)?);
        let expression = inner.expression()?;
        inner.expect_end()?;
        Ok(expression)
    }

    fn axis(&mut self) -> Parsed<Axis> {
        let non_empty = self.word("NON");
        if non_empty {
            self.expect_word("EMPTY")?;
        }
        let set = self.expression()?;
        let mut properties = Vec::new();
        self.word("DIMENSION");
        if self.word("PROPERTIES") {
            loop {
                let Expr::Name(name) = self.name()? else {
                    unreachable!("a name is a name");
                };
                properties.push(name);
                if !self.symbol(",") {
                    break;
                }
            }
        }
        self.expect_word("ON")?;
        let named = AXES.iter().position(|name| self.word(name));
        let ordinal = if let Some(ordinal) = named {
            ordinal
        } else {
            let wrapped = self.word("AXIS");
            if wrapped {
                self.expect_symbol("(")?;
            }
            let Token::Number(number) = self.advance() else {
                return Err(self.error("expected COLUMNS, ROWS or an axis number"));
            };
            if wrapped {
                self.expect_symbol(")")?;
            }
            number as usize
        };
        Ok(Axis {
            non_empty,
            set,
            properties,
            ordinal,
        })
    }

    fn expression(&mut self) -> Parsed<Expr> {
        self.binary(0)
    }

    fn binary(&mut self, level: usize) -> Parsed<Expr> {
        if level == LEVELS.len() {
            return self.unary();
        }
        if level == COMPARISONS && self.word("NOT") {
            return Ok(Expr::Not(Box::new(self.binary(level)?)));
        }
        let mut left = self.binary(level + 1)?;
        while let Some(operator) = self.operator(LEVELS[level]) {
            let right = self.binary(level + 1)?;
            left = Expr::Binary(Box::new(left), operator, Box::new(right));
        }
        Ok(left)
    }

    /// Consumes the next token if it is one of the operators, and says which.
    fn operator(&mut self, operators: &[&'static str]) -> Option<&'static str> {
        let found = operators
            .iter()
            .copied()
            .find(|operator| match self.peek() {
                Token::Symbol(symbol) => symbol == operator,
                Token::Word(word) => word.eq_ignore_ascii_case(operator),
                _ => false,
            });
        self.next += usize::from(found.is_some());
        found
    }

    fn unary(&mut self) -> Parsed<Expr> {
        if self.symbol("-") {
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        let mut expression = self.primary()?;
        // Methods: a dot followed by a word.
        while self.symbol(".") {
            let Token::Word(method) = self.advance() else {
                return Err(self.error("expected a name or a method after '.'"));
            };
            let arguments = match self.symbol("(") {
                true => self.list(")")?,
                false => Vec::new(),
            };
            expression = Expr::Method(Box::new(expression), method.to_uppercase(), arguments);
        }
        // `first : last` is the range of members from one to the other.
        if self.symbol(":") {
            let last = self.unary()?;
            expression = Expr::Binary(Box::new(expression), ":", Box::new(last));
        }
        Ok(expression)
    }

    fn primary(&mut self) -> Parsed<Expr> {
        let called = matches!(self.after(), Token::Symbol("("));
        match self.peek().clone() {
            Token::Number(number) => {
                self.advance();
                Ok(Expr::Number(number))
            }
            Token::Text(text) => {
                self.advance();
                Ok(Expr::Text(text))
            }
            Token::Symbol("{") => {
                self.advance();
                Ok(Expr::Set(self.list("}")?))
            }
            Token::Symbol("(") => {
                self.advance();
                let mut items = self.list(")")?;
                match items.len() {
                    0 => Err(self.error("expected an expression inside '()'")),
                    1 => Ok(items.remove(0)),
                    _ => Ok(Expr::Tuple(items)),
                }
            }
            Token::Word(function) if called => {
                self.next += 2;
                Ok(Expr::Call(function.to_uppercase(), self.list(")")?))
            }
            Token::Word(_) | Token::Name(_) => self.name(),
            _ => Err(self.error("expected an expression")),
        }
    }

    /// A dotted name. It stops before a dot that introduces a method.
    fn name(&mut self) -> Parsed<Expr> {
        let mut parts = Vec::new();
        loop {
            match self.advance() {
                Token::Name(part) | Token::Word(part) => parts.push(part),
                _ => return Err(self.error("expected a name")),
            }
            let continues = matches!(self.peek(), Token::Symbol("."))
                && match self.after() {
                    Token::Name(_) => true,
                    Token::Word(word) => !METHODS.iter().any(|m| word.eq_ignore_ascii_case(m)),
                    _ => false,
                };
            if !continues {
                return Ok(Expr::Name(parts));
            }
            self.advance();
        }
    }

    /// Comma-separated expressions up to a closing symbol, which is consumed.
    fn list(&mut self, close: &'static str) -> Parsed<Vec<Expr>> {
        let mut items = Vec::new();
        while !self.symbol(close) {
            if !items.is_empty() {
                self.expect_symbol(",")?;
            }
            items.push(self.expression()?);
        }
        Ok(items)
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.next].0
    }

    /// The token after the next one.
    fn after(&self) -> &Token {
        let last = self.tokens.len() - 1;
        &self.tokens[(self.next + 1).min(last)].0
    }

    /// Takes the next token. The final `End` token is never passed.
    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        self.next = (self.next + 1).min(self.tokens.len() - 1);
        token
    }

    /// Consumes the next token if it is the given word, in any case.
    fn word(&mut self, word: &str) -> bool {
        let found = matches!(self.peek(), Token::Word(next) if next.eq_ignore_ascii_case(word));
        self.next += usize::from(found);
        found
    }

    fn symbol(&mut self, symbol: &str) -> bool {
        let found = matches!(self.peek(), Token::Symbol(next) if *next == symbol);
        self.next += usize::from(found);
        found
    }

    fn expect_word(&mut self, word: &str) -> Parsed<()> {
        match self.word(word) {
            true => Ok(()),
            false => Err(self.error(&format!("expected {word}"))),
        }
    }

    fn expect_symbol(&mut self, symbol: &str) -> Parsed<()> {
        match self.symbol(symbol) {
            true => Ok(()),
            false => Err(self.error(&format!("expected '{symbol}'"))),
        }
    }

    fn expect_end(&mut self) -> Parsed<()> {
        match self.peek() {
            Token::End => Ok(()),
            _ => Err(self.error("unexpected text after the end of the query")),
        }
    }

    /// An error at the token about to be read.
    fn error(&self, message: &str) -> MdxError {
        MdxError {
            message: message.to_owned(),
            position: Some(self.tokens[self.next].1),
        }
    }
}
