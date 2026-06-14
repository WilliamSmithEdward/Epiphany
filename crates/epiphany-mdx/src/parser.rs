//! Recursive-descent parser for the MDX set sublanguage.
//!
//! Grammar (set expressions; keywords are case-insensitive and bracket a name
//! to use a reserved word as a member, e.g. `[Filter]`):
//!
//! ```text
//! set        := crossjoin
//! crossjoin  := primary ( '*' primary )*
//! primary    := '{' ( set ( ',' set )* )? '}'
//!             | 'Filter'     '(' set ',' predicate ')'
//!             | 'Order'      '(' set ',' attr ( ',' dir )? ')'
//!             | 'Crossjoin'  '(' set ',' set ')'
//!             | 'Descendants' '(' member ')'
//!             | member ( '.Members' | '.Children' | '.Descendants' )?
//! member     := name ( '.' name )*
//! predicate  := or
//! or         := and ( 'OR' and )*
//! and        := not ( 'AND' not )*
//! not        := 'NOT' not | '(' predicate ')' | comparison
//! comparison := operand cmp operand
//! operand    := string | number | <path> '.Properties' '(' string ')'
//! ```

use crate::ast::{CmpOp, MemberRef, Operand, OrderDir, Predicate, SetExpr};
use crate::error::{MdxParseError, ParseErrorKind};
use crate::lexer::{lex, Span, Tok, Token};

/// Parse a set expression from source text.
///
/// Returns the [`SetExpr`] AST, or an [`MdxParseError`] carrying the byte span
/// of the first lex or parse failure.
pub fn parse(src: &str) -> Result<SetExpr, MdxParseError> {
    let toks = lex(src)?;
    let mut parser = Parser {
        toks,
        pos: 0,
        end: src.len(),
    };
    let expr = parser.parse_set()?;
    if parser.pos < parser.toks.len() {
        return Err(MdxParseError::new(
            ParseErrorKind::TrailingInput,
            parser.toks[parser.pos].span,
        ));
    }
    Ok(expr)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.toks.get(self.pos + 1).map(|t| &t.tok)
    }

    fn bump(&mut self) -> Tok {
        let tok = self.toks[self.pos].tok.clone();
        self.pos += 1;
        tok
    }

    /// Span of the current token, or an empty span at end-of-input.
    fn here_span(&self) -> Span {
        self.toks
            .get(self.pos)
            .map(|t| t.span)
            .unwrap_or(Span::new(self.end, self.end))
    }

    fn unexpected(&self, expected: &'static str) -> MdxParseError {
        match self.peek() {
            Some(tok) => MdxParseError::new(
                ParseErrorKind::UnexpectedToken {
                    found: tok.describe(),
                    expected,
                },
                self.here_span(),
            ),
            None => MdxParseError::new(
                ParseErrorKind::UnexpectedEof { expected },
                Span::new(self.end, self.end),
            ),
        }
    }

    fn expect(&mut self, want: &Tok, expected: &'static str) -> Result<(), MdxParseError> {
        if self.peek() == Some(want) {
            self.bump();
            Ok(())
        } else {
            Err(self.unexpected(expected))
        }
    }

    /// Consume a name token (bare or bracketed) and return its text.
    fn expect_name(&mut self, expected: &'static str) -> Result<String, MdxParseError> {
        match self.peek() {
            Some(Tok::Name { text, .. }) => {
                let text = text.clone();
                self.bump();
                Ok(text)
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    // --- set expressions ---

    fn parse_set(&mut self) -> Result<SetExpr, MdxParseError> {
        let mut left = self.parse_primary()?;
        while self.peek() == Some(&Tok::Star) {
            self.bump();
            let right = self.parse_primary()?;
            left = SetExpr::Crossjoin(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<SetExpr, MdxParseError> {
        match self.peek() {
            Some(Tok::LBrace) => self.parse_set_literal(),
            Some(Tok::Name {
                text,
                bracketed: false,
            }) if is_func_kw(text) && self.peek2() == Some(&Tok::LParen) => {
                let name = text.to_ascii_lowercase();
                self.bump(); // function name
                self.bump(); // '('
                self.parse_func_body(&name)
            }
            Some(Tok::Name { .. }) => self.parse_member_or_postfix(),
            _ => Err(self.unexpected("a set expression")),
        }
    }

    fn parse_set_literal(&mut self) -> Result<SetExpr, MdxParseError> {
        self.expect(&Tok::LBrace, "`{`")?;
        if self.peek() == Some(&Tok::RBrace) {
            self.bump();
            return Ok(SetExpr::Set(Vec::new()));
        }
        let mut items = vec![self.parse_set()?];
        while self.peek() == Some(&Tok::Comma) {
            self.bump();
            items.push(self.parse_set()?);
        }
        self.expect(&Tok::RBrace, "`,` or `}`")?;
        Ok(SetExpr::Set(items))
    }

    fn parse_func_body(&mut self, name: &str) -> Result<SetExpr, MdxParseError> {
        match name {
            "filter" => {
                let set = self.parse_set()?;
                self.expect(&Tok::Comma, "`,`")?;
                let pred = self.parse_predicate()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(SetExpr::Filter(Box::new(set), pred))
            }
            "order" => {
                let set = self.parse_set()?;
                self.expect(&Tok::Comma, "`,`")?;
                let attr = self.parse_order_key()?;
                let dir = if self.peek() == Some(&Tok::Comma) {
                    self.bump();
                    self.parse_dir()?
                } else {
                    OrderDir::Asc
                };
                self.expect(&Tok::RParen, "`)`")?;
                Ok(SetExpr::Order(Box::new(set), attr, dir))
            }
            "crossjoin" => {
                let a = self.parse_set()?;
                self.expect(&Tok::Comma, "`,`")?;
                let b = self.parse_set()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(SetExpr::Crossjoin(Box::new(a), Box::new(b)))
            }
            "descendants" => {
                let member = self.parse_member_ref()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(SetExpr::Descendants(member))
            }
            _ => unreachable!("parse_func_body called with non-function keyword"),
        }
    }

    /// Parse a member path followed by an optional postfix set function.
    fn parse_member_or_postfix(&mut self) -> Result<SetExpr, MdxParseError> {
        let mut path = vec![self.expect_name("a member name")?];
        while self.peek() == Some(&Tok::Dot) {
            self.bump(); // '.'
            match self.peek() {
                Some(Tok::Name {
                    text,
                    bracketed: false,
                }) if is_postfix_kw(text) => {
                    let kw = text.to_ascii_lowercase();
                    self.bump();
                    let member = MemberRef::new(path);
                    return Ok(match kw.as_str() {
                        "members" => SetExpr::Members(member),
                        "children" => SetExpr::Children(member),
                        "descendants" => SetExpr::Descendants(member),
                        _ => unreachable!(),
                    });
                }
                Some(Tok::Name { .. }) => {
                    path.push(self.expect_name("a member name")?);
                }
                _ => {
                    return Err(self
                        .unexpected("a member name or `.Members` / `.Children` / `.Descendants`"));
                }
            }
        }
        Ok(SetExpr::Member(MemberRef::new(path)))
    }

    /// Parse a bare member path (no postfix), used as a function argument.
    fn parse_member_ref(&mut self) -> Result<MemberRef, MdxParseError> {
        let mut path = vec![self.expect_name("a member name")?];
        while self.peek() == Some(&Tok::Dot) && matches!(self.peek2(), Some(Tok::Name { .. })) {
            self.bump(); // '.'
            path.push(self.expect_name("a member name")?);
        }
        Ok(MemberRef::new(path))
    }

    fn parse_order_key(&mut self) -> Result<String, MdxParseError> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.bump();
                Ok(s)
            }
            Some(Tok::Name { text, .. }) => {
                let text = text.clone();
                self.bump();
                Ok(text)
            }
            _ => Err(self.unexpected("an attribute name")),
        }
    }

    fn parse_dir(&mut self) -> Result<OrderDir, MdxParseError> {
        match self.peek() {
            Some(Tok::Name {
                text,
                bracketed: false,
            }) => {
                let dir = match text.to_ascii_lowercase().as_str() {
                    "asc" => OrderDir::Asc,
                    "desc" => OrderDir::Desc,
                    "basc" => OrderDir::BAsc,
                    "bdesc" => OrderDir::BDesc,
                    _ => return Err(self.unexpected("ASC, DESC, BASC, or BDESC")),
                };
                self.bump();
                Ok(dir)
            }
            _ => Err(self.unexpected("ASC, DESC, BASC, or BDESC")),
        }
    }

    // --- predicates ---

    fn parse_predicate(&mut self) -> Result<Predicate, MdxParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Predicate, MdxParseError> {
        let mut left = self.parse_and()?;
        while self.peek_keyword("or") {
            self.bump();
            let right = self.parse_and()?;
            left = Predicate::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Predicate, MdxParseError> {
        let mut left = self.parse_not()?;
        while self.peek_keyword("and") {
            self.bump();
            let right = self.parse_not()?;
            left = Predicate::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Predicate, MdxParseError> {
        if self.peek_keyword("not") {
            self.bump();
            Ok(Predicate::Not(Box::new(self.parse_not()?)))
        } else if self.peek() == Some(&Tok::LParen) {
            self.bump();
            let pred = self.parse_predicate()?;
            self.expect(&Tok::RParen, "`)`")?;
            Ok(pred)
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Predicate, MdxParseError> {
        let left = self.parse_operand()?;
        let op = self.parse_cmp_op()?;
        let right = self.parse_operand()?;
        Ok(Predicate::Compare { left, op, right })
    }

    fn parse_cmp_op(&mut self) -> Result<CmpOp, MdxParseError> {
        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Ge) => CmpOp::Ge,
            _ => return Err(self.unexpected("a comparison operator")),
        };
        self.bump();
        Ok(op)
    }

    fn parse_operand(&mut self) -> Result<Operand, MdxParseError> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.bump();
                Ok(Operand::Str(s))
            }
            Some(Tok::Number(n)) => {
                let n = n.clone();
                self.bump();
                Ok(Operand::Number(n))
            }
            Some(Tok::Name { .. }) => self.parse_property_operand(),
            _ => Err(self.unexpected("an attribute property, string, or number")),
        }
    }

    /// Parse a `<path>.Properties("Attr")` operand. The leading path
    /// (`[Dim].CurrentMember`, etc.) is contextual sugar; only the attribute
    /// name inside `Properties(...)` is retained.
    fn parse_property_operand(&mut self) -> Result<Operand, MdxParseError> {
        let mut last = self.expect_name("a member reference")?;
        while self.peek() == Some(&Tok::Dot) && matches!(self.peek2(), Some(Tok::Name { .. })) {
            self.bump(); // '.'
            last = self.expect_name("a member reference")?;
        }
        if !last.eq_ignore_ascii_case("properties") {
            return Err(self.unexpected("`.Properties(\"Attr\")`"));
        }
        self.expect(&Tok::LParen, "`(`")?;
        let attr = match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.bump();
                s
            }
            _ => return Err(self.unexpected("a quoted attribute name")),
        };
        self.expect(&Tok::RParen, "`)`")?;
        Ok(Operand::Property(attr))
    }

    fn peek_keyword(&self, kw: &str) -> bool {
        matches!(
            self.peek(),
            Some(Tok::Name { text, bracketed: false }) if text.eq_ignore_ascii_case(kw)
        )
    }
}

fn is_func_kw(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "filter" | "order" | "crossjoin" | "descendants"
    )
}

fn is_postfix_kw(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "members" | "children" | "descendants"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(segments: &[&str]) -> MemberRef {
        MemberRef::new(segments.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parses_a_bare_member() {
        assert_eq!(
            parse("[Region].[North]").unwrap(),
            SetExpr::Member(m(&["Region", "North"]))
        );
    }

    #[test]
    fn parses_postfix_set_functions() {
        assert_eq!(
            parse("[Region].Members").unwrap(),
            SetExpr::Members(m(&["Region"]))
        );
        assert_eq!(
            parse("[Region].[Total].Children").unwrap(),
            SetExpr::Children(m(&["Region", "Total"]))
        );
        assert_eq!(
            parse("[Region].[Total].Descendants").unwrap(),
            SetExpr::Descendants(m(&["Region", "Total"]))
        );
    }

    #[test]
    fn descendants_function_form_matches_postfix() {
        assert_eq!(
            parse("Descendants([Region].[Total])").unwrap(),
            SetExpr::Descendants(m(&["Region", "Total"]))
        );
    }

    #[test]
    fn parses_set_literal_with_nested_crossjoin() {
        let parsed = parse("{ [Region].[North], [Region].[South] }").unwrap();
        assert_eq!(
            parsed,
            SetExpr::Set(vec![
                SetExpr::Member(m(&["Region", "North"])),
                SetExpr::Member(m(&["Region", "South"])),
            ])
        );
    }

    #[test]
    fn empty_set_literal() {
        assert_eq!(parse("{}").unwrap(), SetExpr::Set(Vec::new()));
    }

    #[test]
    fn crossjoin_infix_is_left_associative() {
        let parsed = parse("[A].Members * [B].Members * [C].Members").unwrap();
        let expected = SetExpr::Crossjoin(
            Box::new(SetExpr::Crossjoin(
                Box::new(SetExpr::Members(m(&["A"]))),
                Box::new(SetExpr::Members(m(&["B"]))),
            )),
            Box::new(SetExpr::Members(m(&["C"]))),
        );
        assert_eq!(parsed, expected);
        // The function form is equivalent.
        assert_eq!(
            parse("Crossjoin(Crossjoin([A].Members, [B].Members), [C].Members)").unwrap(),
            expected
        );
    }

    #[test]
    fn parses_filter_with_attribute_predicate() {
        let parsed = parse("Filter([Region].Members, Properties(\"Code\") = \"N\")").unwrap();
        assert_eq!(
            parsed,
            SetExpr::Filter(
                Box::new(SetExpr::Members(m(&["Region"]))),
                Predicate::Compare {
                    left: Operand::Property("Code".to_string()),
                    op: CmpOp::Eq,
                    right: Operand::Str("N".to_string()),
                }
            )
        );
    }

    #[test]
    fn filter_with_and_or_not_and_qualified_property() {
        let parsed = parse(
            "Filter([Region].Members, NOT [Region].CurrentMember.Properties(\"Hidden\") = \"yes\" AND Properties(\"Pop\") > 100)",
        )
        .unwrap();
        let SetExpr::Filter(_, pred) = parsed else {
            panic!("expected Filter");
        };
        assert_eq!(
            pred,
            Predicate::And(
                Box::new(Predicate::Not(Box::new(Predicate::Compare {
                    left: Operand::Property("Hidden".to_string()),
                    op: CmpOp::Eq,
                    right: Operand::Str("yes".to_string()),
                }))),
                Box::new(Predicate::Compare {
                    left: Operand::Property("Pop".to_string()),
                    op: CmpOp::Gt,
                    right: Operand::Number("100".to_string()),
                }),
            )
        );
    }

    #[test]
    fn parses_order_with_and_without_direction() {
        assert_eq!(
            parse("Order([Region].Members, \"Code\", DESC)").unwrap(),
            SetExpr::Order(
                Box::new(SetExpr::Members(m(&["Region"]))),
                "Code".to_string(),
                OrderDir::Desc
            )
        );
        assert_eq!(
            parse("Order([Region].Members, [Code])").unwrap(),
            SetExpr::Order(
                Box::new(SetExpr::Members(m(&["Region"]))),
                "Code".to_string(),
                OrderDir::Asc
            )
        );
    }

    #[test]
    fn reserved_word_as_member_via_brackets() {
        // A member literally named "Members" must be bracketed.
        assert_eq!(parse("[Filter]").unwrap(), SetExpr::Member(m(&["Filter"])));
        assert_eq!(
            parse("[Dim].[Members]").unwrap(),
            SetExpr::Member(m(&["Dim", "Members"]))
        );
    }

    #[test]
    fn round_trips_through_pretty_print() {
        let corpus = [
            "[Region].[North]",
            "[Region].Members",
            "[Region].[Total].Children",
            "Descendants([Region].[Total])",
            "{[Region].[North], [Region].[South]}",
            "{}",
            "Crossjoin([A].Members, [B].Members)",
            "[A].Members * [B].Members",
            "Filter([Region].Members, Properties(\"Code\") = \"N\")",
            "Order([Region].Members, \"Code\", DESC)",
            "Filter([Region].Members, NOT Properties(\"Hidden\") = \"yes\" AND Properties(\"Pop\") > 100)",
        ];
        for src in corpus {
            let first = parse(src).unwrap();
            let printed = first.to_string();
            let second =
                parse(&printed).unwrap_or_else(|e| panic!("re-parse of `{printed}` failed: {e}"));
            assert_eq!(
                first, second,
                "round-trip changed AST for `{src}` -> `{printed}`"
            );
        }
    }

    #[test]
    fn error_table_reports_kind_and_span() {
        // Trailing input after a complete expression.
        let err = parse("[A] [B]").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::TrailingInput);
        assert_eq!(err.span, Span::new(4, 7));

        // Missing closing brace -> EOF where `,` or `}` expected.
        let err = parse("{[A]").unwrap_err();
        assert_eq!(
            err.kind,
            ParseErrorKind::UnexpectedEof {
                expected: "`,` or `}`"
            }
        );

        // A bad postfix after a dot.
        let err = parse("[A].123").unwrap_err();
        assert!(matches!(err.kind, ParseErrorKind::UnexpectedToken { .. }));

        // Empty input.
        let err = parse("   ").unwrap_err();
        assert_eq!(
            err.kind,
            ParseErrorKind::UnexpectedEof {
                expected: "a set expression"
            }
        );
    }

    #[test]
    fn parse_is_deterministic() {
        let src = "Filter(Crossjoin([A].Members, [B].[T].Children), Properties(\"x\") <= 3)";
        assert_eq!(parse(src).unwrap(), parse(src).unwrap());
    }
}
