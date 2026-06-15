//! Recursive-descent parser for the rules language.
//!
//! Grammar (keywords are case-insensitive; names and strings are single-quoted):
//!
//! ```text
//! doc        := rule*
//! rule       := area '=' expr ';'
//! area       := '[' dimsel (',' dimsel)* ']'
//! dimsel     := name ':' selector
//! selector   := name                                  // element
//!             | '@' name cmp literal                  // attribute predicate
//!             | '{' family '}'
//! family     := 'leaves' | 'consolidated' | 'all'
//!             | 'children' 'of' name | 'descendants' 'of' name
//! expr       := add
//! add        := mul ( ('+'|'-') mul )*
//! mul        := unary ( ('*'|'/') unary )*
//! unary      := '-' unary | primary
//! primary    := number | string | '(' expr ')' | cellref
//!             | 'IF' condition 'THEN' expr ('ELSE' expr)?
//!             | func '(' (arg (',' arg)*)? ')'
//! cellref    := 'value' overrides? | overrides | name '!' overrides mapping?
//! overrides  := '[' (name ':' member (',' name ':' member)*) ']'
//! member     := name | '!' name
//! mapping    := 'with' '(' name '->' name (',' name '->' name)* ')'
//! condition  := or ;  or := and ('OR' and)* ;  and := not ('AND' not)*
//! not        := 'NOT' not | compare ;  compare := expr cmp expr
//! ```

use std::collections::HashSet;

use crate::rules::ast::{
    Area, ArithOp, BuiltinFunc, CellRef, CmpOp, Condition, DimOverride, DimSelector, Expr, FuncArg,
    FuncCall, Literal, MemberExpr, Rule, RuleDoc, SelectorKind,
};
use crate::rules::error::{ParseErrorKind, RuleParseError};
use crate::rules::lexer::{lex, Span, Tok, Token};

/// Parse a rules document (zero or more `area = formula ;` statements).
pub fn parse(src: &str) -> Result<RuleDoc, RuleParseError> {
    let toks = lex(src)?;
    let mut parser = Parser {
        toks,
        pos: 0,
        end: src.len(),
        depth: 0,
    };
    let mut rules = Vec::new();
    while parser.pos < parser.toks.len() {
        rules.push(parser.parse_rule()?);
    }
    Ok(RuleDoc { rules })
}

/// Parse exactly one rule statement, rejecting trailing input.
pub fn parse_rule(src: &str) -> Result<Rule, RuleParseError> {
    let toks = lex(src)?;
    let mut parser = Parser {
        toks,
        pos: 0,
        end: src.len(),
        depth: 0,
    };
    let rule = parser.parse_rule()?;
    if parser.pos < parser.toks.len() {
        return Err(RuleParseError::new(
            ParseErrorKind::TrailingInput,
            parser.toks[parser.pos].span,
        ));
    }
    Ok(rule)
}

/// Maximum nesting depth for expressions and conditions. A stack-exhaustion
/// guard, not a real limit: hand-authored rules nest a handful of levels deep,
/// far below this. A fixed safety backstop, not an operational tuning knob.
const MAX_PARSE_DEPTH: usize = 128;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    end: usize,
    /// Current recursion depth, bracketed by [`enter`](Parser::enter)/[`leave`](Parser::leave)
    /// across `parse_expr`/`parse_unary`/`parse_not` so siblings stay shallow and
    /// only true nesting accumulates.
    depth: usize,
}

impl Parser {
    /// Enter one level of recursion; error if it would exceed [`MAX_PARSE_DEPTH`].
    fn enter(&mut self) -> Result<(), RuleParseError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(RuleParseError::new(
                ParseErrorKind::TooDeep,
                self.here_span(),
            ));
        }
        Ok(())
    }

    /// Leave one level of recursion (bracketed with [`enter`](Parser::enter)).
    fn leave(&mut self) {
        self.depth -= 1;
    }

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

    fn here_span(&self) -> Span {
        self.toks
            .get(self.pos)
            .map(|t| t.span)
            .unwrap_or(Span::new(self.end, self.end))
    }

    fn prev_end(&self) -> usize {
        self.toks[self.pos - 1].span.end
    }

    fn unexpected(&self, expected: &'static str) -> RuleParseError {
        match self.peek() {
            Some(tok) => RuleParseError::new(
                ParseErrorKind::UnexpectedToken {
                    found: tok.describe(),
                    expected,
                },
                self.here_span(),
            ),
            None => RuleParseError::new(
                ParseErrorKind::UnexpectedEof { expected },
                Span::new(self.end, self.end),
            ),
        }
    }

    fn expect(&mut self, want: &Tok, expected: &'static str) -> Result<(), RuleParseError> {
        if self.peek() == Some(want) {
            self.bump();
            Ok(())
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn expect_quoted(&mut self, expected: &'static str) -> Result<String, RuleParseError> {
        match self.peek() {
            Some(Tok::Quoted(s)) => {
                let s = s.clone();
                self.bump();
                Ok(s)
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    fn peek_word(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    fn peek2_is(&self, want: &Tok) -> bool {
        self.peek2() == Some(want)
    }

    fn eat_word(&mut self, kw: &str) -> bool {
        if self.peek_word(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, kw: &str, expected: &'static str) -> Result<(), RuleParseError> {
        if self.eat_word(kw) {
            Ok(())
        } else {
            Err(self.unexpected(expected))
        }
    }

    // --- rules / areas ---

    fn parse_rule(&mut self) -> Result<Rule, RuleParseError> {
        let start = self.here_span().start;
        let area = self.parse_area()?;
        self.expect(&Tok::Eq, "`=`")?;
        let formula = self.parse_expr()?;
        self.expect(&Tok::Semicolon, "`;`")?;
        Ok(Rule {
            area,
            formula,
            span: Span::new(start, self.prev_end()),
        })
    }

    fn parse_area(&mut self) -> Result<Area, RuleParseError> {
        self.expect(&Tok::LBracket, "`[` to open an area")?;
        let mut selectors = Vec::new();
        let mut seen = HashSet::new();
        loop {
            let start = self.here_span().start;
            let dimension = self.expect_quoted("a dimension name")?;
            self.expect(&Tok::Colon, "`:`")?;
            let kind = self.parse_selector()?;
            let span = Span::new(start, self.prev_end());
            if !seen.insert(dimension.clone()) {
                return Err(RuleParseError::new(
                    ParseErrorKind::DuplicateDimension(dimension),
                    span,
                ));
            }
            selectors.push(DimSelector {
                dimension,
                kind,
                span,
            });
            if self.peek() == Some(&Tok::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(&Tok::RBracket, "`,` or `]`")?;
        Ok(Area { selectors })
    }

    fn parse_selector(&mut self) -> Result<SelectorKind, RuleParseError> {
        match self.peek() {
            Some(Tok::At) => {
                self.bump();
                let attribute = self.expect_quoted("an attribute name")?;
                let op = self.parse_cmp_op()?;
                let value = self.parse_literal()?;
                Ok(SelectorKind::AttrPredicate {
                    attribute,
                    op,
                    value,
                })
            }
            Some(Tok::LBrace) => {
                self.bump();
                let kind = self.parse_family()?;
                self.expect(&Tok::RBrace, "`}`")?;
                Ok(kind)
            }
            Some(Tok::Quoted(name)) => {
                let name = name.clone();
                self.bump();
                Ok(SelectorKind::Element(name))
            }
            _ => Err(self.unexpected(
                "a selector (an element name, `@`attribute, or `{leaves}`/`{all}`/...)",
            )),
        }
    }

    fn parse_family(&mut self) -> Result<SelectorKind, RuleParseError> {
        if self.eat_word("leaves") {
            Ok(SelectorKind::Leaves)
        } else if self.eat_word("consolidated") {
            Ok(SelectorKind::Consolidated)
        } else if self.eat_word("all") {
            Ok(SelectorKind::All)
        } else if self.eat_word("children") {
            self.expect_word("of", "`of`")?;
            Ok(SelectorKind::Children(
                self.expect_quoted("an element name")?,
            ))
        } else if self.eat_word("descendants") {
            self.expect_word("of", "`of`")?;
            Ok(SelectorKind::Descendants(
                self.expect_quoted("an element name")?,
            ))
        } else {
            Err(self.unexpected("leaves, consolidated, all, children of, or descendants of"))
        }
    }

    fn parse_literal(&mut self) -> Result<Literal, RuleParseError> {
        match self.peek() {
            Some(Tok::Number(n)) => {
                let n = n.clone();
                self.bump();
                Ok(Literal::Number(n))
            }
            Some(Tok::Quoted(s)) => {
                let s = s.clone();
                self.bump();
                Ok(Literal::Str(s))
            }
            _ => Err(self.unexpected("a string or number literal")),
        }
    }

    fn parse_cmp_op(&mut self) -> Result<CmpOp, RuleParseError> {
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

    // --- value expressions ---

    fn parse_expr(&mut self) -> Result<Expr, RuleParseError> {
        // Depth is bracketed in parse_unary (every expression flows through it,
        // covering both parenthesis nesting and unary chains) and in the condition
        // parse_not, so parse_expr itself does not double-count.
        self.parse_add()
    }

    fn parse_add(&mut self) -> Result<Expr, RuleParseError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => ArithOp::Add,
                Some(Tok::Minus) => ArithOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.parse_mul()?;
            left = Expr::Bin {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Expr, RuleParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => ArithOp::Mul,
                Some(Tok::Slash) => ArithOp::Div,
                _ => break,
            };
            self.bump();
            let right = self.parse_unary()?;
            left = Expr::Bin {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, RuleParseError> {
        self.enter()?;
        let result = self.parse_unary_inner();
        self.leave();
        result
    }

    fn parse_unary_inner(&mut self) -> Result<Expr, RuleParseError> {
        if self.peek() == Some(&Tok::Minus) {
            self.bump();
            Ok(Expr::Neg(Box::new(self.parse_unary()?)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, RuleParseError> {
        match self.peek() {
            Some(Tok::Number(n)) => {
                let n = n.clone();
                self.bump();
                Ok(Expr::Number(n))
            }
            Some(Tok::LParen) => {
                self.bump();
                let inner = self.parse_expr()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(inner)
            }
            Some(Tok::LBracket) => Ok(Expr::Cell(self.parse_overrides_cellref()?)),
            Some(Tok::Quoted(s)) => {
                if self.peek2_is(&Tok::Bang) {
                    Ok(Expr::Cell(self.parse_cross_cube_ref()?))
                } else {
                    let s = s.clone();
                    self.bump();
                    Ok(Expr::Str(s))
                }
            }
            Some(Tok::Word(w)) => {
                let lw = w.to_ascii_lowercase();
                if lw == "value" {
                    Ok(Expr::Cell(self.parse_value_ref()?))
                } else if lw == "if" {
                    self.parse_if()
                } else if self.peek2_is(&Tok::LParen) {
                    self.parse_func()
                } else {
                    Err(self.unexpected("a value (number, name, `value`, IF, function, or `(`)"))
                }
            }
            _ => Err(self.unexpected("a value (number, name, `value`, IF, function, or `(`)")),
        }
    }

    fn parse_if(&mut self) -> Result<Expr, RuleParseError> {
        self.expect_word("if", "IF")?;
        let cond = self.parse_condition()?;
        self.expect_word("then", "THEN")?;
        let then = self.parse_expr()?;
        let otherwise = if self.eat_word("else") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        Ok(Expr::If {
            cond: Box::new(cond),
            then: Box::new(then),
            otherwise,
        })
    }

    fn parse_func(&mut self) -> Result<Expr, RuleParseError> {
        let start = self.here_span().start;
        let word = match self.bump() {
            Tok::Word(w) => w,
            _ => unreachable!("parse_func entered without a word"),
        };
        let func = BuiltinFunc::from_word(&word).ok_or_else(|| {
            RuleParseError::new(
                ParseErrorKind::UnknownFunction(word.clone()),
                Span::new(start, self.prev_end()),
            )
        })?;
        self.expect(&Tok::LParen, "`(`")?;
        let mut args = Vec::new();
        if self.peek() != Some(&Tok::RParen) {
            loop {
                match self.peek() {
                    Some(Tok::Quoted(s)) => {
                        let s = s.clone();
                        // A quoted token followed by `!` is a cross-cube ref, not a name arg.
                        if self.peek2_is(&Tok::Bang) {
                            args.push(FuncArg::Expr(self.parse_expr()?));
                        } else {
                            self.bump();
                            args.push(FuncArg::Str(s));
                        }
                    }
                    _ => args.push(FuncArg::Expr(self.parse_expr()?)),
                }
                if self.peek() == Some(&Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen, "`,` or `)`")?;
        Ok(Expr::Func(FuncCall {
            func,
            args,
            span: Span::new(start, self.prev_end()),
        }))
    }

    // --- cell references ---

    fn parse_value_ref(&mut self) -> Result<CellRef, RuleParseError> {
        let start = self.here_span().start;
        self.expect_word("value", "`value`")?;
        let overrides = if self.peek() == Some(&Tok::LBracket) {
            self.parse_overrides()?
        } else {
            Vec::new()
        };
        Ok(CellRef {
            cube: None,
            overrides,
            mapping: Vec::new(),
            span: Span::new(start, self.prev_end()),
        })
    }

    fn parse_overrides_cellref(&mut self) -> Result<CellRef, RuleParseError> {
        let start = self.here_span().start;
        let overrides = self.parse_overrides()?;
        Ok(CellRef {
            cube: None,
            overrides,
            mapping: Vec::new(),
            span: Span::new(start, self.prev_end()),
        })
    }

    fn parse_cross_cube_ref(&mut self) -> Result<CellRef, RuleParseError> {
        let start = self.here_span().start;
        let cube = self.expect_quoted("a cube name")?;
        self.expect(&Tok::Bang, "`!`")?;
        let overrides = self.parse_overrides()?;
        let mapping = if self.peek_word("with") {
            self.parse_mapping()?
        } else {
            Vec::new()
        };
        Ok(CellRef {
            cube: Some(cube),
            overrides,
            mapping,
            span: Span::new(start, self.prev_end()),
        })
    }

    fn parse_overrides(&mut self) -> Result<Vec<DimOverride>, RuleParseError> {
        self.expect(&Tok::LBracket, "`[`")?;
        let mut overrides = Vec::new();
        loop {
            let dimension = self.expect_quoted("a dimension name")?;
            self.expect(&Tok::Colon, "`:`")?;
            let member = self.parse_member_expr()?;
            overrides.push(DimOverride { dimension, member });
            if self.peek() == Some(&Tok::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(&Tok::RBracket, "`,` or `]`")?;
        Ok(overrides)
    }

    fn parse_member_expr(&mut self) -> Result<MemberExpr, RuleParseError> {
        if self.peek() == Some(&Tok::Bang) {
            self.bump();
            Ok(MemberExpr::Attr(self.expect_quoted("an attribute name")?))
        } else {
            Ok(MemberExpr::Element(self.expect_quoted("a member name")?))
        }
    }

    fn parse_mapping(&mut self) -> Result<Vec<(String, String)>, RuleParseError> {
        self.expect_word("with", "`with`")?;
        self.expect(&Tok::LParen, "`(`")?;
        let mut mapping = Vec::new();
        loop {
            let src = self.expect_quoted("a source dimension name")?;
            self.expect(&Tok::Arrow, "`->`")?;
            let dst = self.expect_quoted("a target dimension name")?;
            mapping.push((src, dst));
            if self.peek() == Some(&Tok::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(&Tok::RParen, "`,` or `)`")?;
        Ok(mapping)
    }

    // --- conditions ---

    fn parse_condition(&mut self) -> Result<Condition, RuleParseError> {
        let mut left = self.parse_and()?;
        while self.eat_word("or") {
            let right = self.parse_and()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Condition, RuleParseError> {
        let mut left = self.parse_not()?;
        while self.eat_word("and") {
            let right = self.parse_not()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Condition, RuleParseError> {
        self.enter()?;
        let result = self.parse_not_inner();
        self.leave();
        result
    }

    fn parse_not_inner(&mut self) -> Result<Condition, RuleParseError> {
        if self.eat_word("not") {
            Ok(Condition::Not(Box::new(self.parse_not()?)))
        } else {
            let left = self.parse_expr()?;
            let op = self.parse_cmp_op()?;
            let right = self.parse_expr()?;
            Ok(Condition::Compare { left, op, right })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_determinism::DeterministicRng;

    fn canon(src: &str) -> String {
        parse(src).unwrap().to_string()
    }

    #[test]
    fn deeply_nested_expr_is_rejected_not_overflowed() {
        // Moderate parenthesis nesting parses; an extreme depth is a clean
        // TooDeep error (a stack-exhaustion guard), never a crash.
        let ok = format!("['m':'a'] = {}0{};", "(".repeat(100), ")".repeat(100));
        assert!(parse(&ok).is_ok());
        let deep = format!("['m':'a'] = {}0{};", "(".repeat(300), ")".repeat(300));
        assert_eq!(parse(&deep).unwrap_err().kind, ParseErrorKind::TooDeep);
    }

    #[test]
    fn simple_leaf_rule() {
        assert_eq!(
            canon("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"),
            "['Measure': 'Margin'] = (value['Measure': 'Sales'] - value['Measure': 'Cost']);"
        );
    }

    #[test]
    fn area_selectors() {
        assert_eq!(
            canon("['Region':{leaves}, 'Measure':{children of 'Total'}] = 1;"),
            "['Region': {leaves}, 'Measure': {children of 'Total'}] = 1;"
        );
        assert_eq!(
            canon("['Region': @'Type' = 'Coastal'] = 0;"),
            "['Region': @'Type' = 'Coastal'] = 0;"
        );
        assert_eq!(canon("['M':{all}] = value;"), "['M': {all}] = value;");
        assert!(canon("['M':{consolidated}] = value['M':'Sales'];").contains("{consolidated}"));
    }

    #[test]
    fn precedence_and_unary() {
        assert_eq!(
            canon("['M':'x'] = 1 + 2 * 3;"),
            "['M': 'x'] = (1 + (2 * 3));"
        );
        assert_eq!(
            canon("['M':'x'] = -value + 1;"),
            "['M': 'x'] = (-value + 1);"
        );
        assert_eq!(
            canon("['M':'x'] = (1 + 2) * 3;"),
            "['M': 'x'] = ((1 + 2) * 3);"
        );
    }

    #[test]
    fn if_then_else_and_conditions() {
        let out = canon("['M':'x'] = IF value > 0 AND value < 10 THEN value ELSE 0;");
        assert_eq!(
            out,
            "['M': 'x'] = IF value > 0 AND value < 10 THEN value ELSE 0;"
        );
        let no_else = canon("['M':'x'] = IF NOT value = 0 THEN 1;");
        assert_eq!(no_else, "['M': 'x'] = IF NOT value = 0 THEN 1;");
    }

    #[test]
    fn functions_and_cross_cube() {
        assert_eq!(
            canon("['M':'x'] = Attr('Region','Code');"),
            "['M': 'x'] = Attr('Region', 'Code');"
        );
        assert_eq!(canon("['M':'x'] = Undef();"), "['M': 'x'] = Undef();");
        assert_eq!(
            canon("['M':'x'] = 'FX'!['Cur':'USD'] with ('M'->'N');"),
            "['M': 'x'] = 'FX'!['Cur': 'USD'] with ('M' -> 'N');"
        );
    }

    #[test]
    fn by_attribute_override_parses() {
        // Deferred at eval (4C rejects), but must parse and round-trip.
        assert_eq!(
            canon("['M':'x'] = value['Region': !'Code'];"),
            "['M': 'x'] = value['Region': !'Code'];"
        );
    }

    #[test]
    fn multiple_rules_in_order() {
        let doc = parse("['M':'a'] = 1;\n['M':'b'] = 2;").unwrap();
        assert_eq!(doc.rules.len(), 2);
        assert_eq!(doc.rules[0].formula.to_string(), "1");
        assert_eq!(doc.rules[1].formula.to_string(), "2");
    }

    #[test]
    fn error_table() {
        // Missing semicolon -> EOF expecting `;`.
        assert_eq!(
            parse("['M':'x'] = 1").unwrap_err().kind,
            ParseErrorKind::UnexpectedEof { expected: "`;`" }
        );
        // Duplicate dimension.
        assert!(matches!(
            parse("['M':'a', 'M':'b'] = 1;").unwrap_err().kind,
            ParseErrorKind::DuplicateDimension(d) if d == "M"
        ));
        // Unknown function.
        assert!(matches!(
            parse("['M':'x'] = Bogus('a');").unwrap_err().kind,
            ParseErrorKind::UnknownFunction(n) if n == "Bogus"
        ));
        // Trailing input on parse_rule.
        assert_eq!(
            parse_rule("['M':'x'] = 1; ['M':'y'] = 2;")
                .unwrap_err()
                .kind,
            ParseErrorKind::TrailingInput
        );
        // A bad selector.
        assert!(matches!(
            parse("['M': 99] = 1;").unwrap_err().kind,
            ParseErrorKind::UnexpectedToken { .. }
        ));
    }

    #[test]
    fn round_trips_through_pretty_print() {
        let corpus = [
            "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];",
            "['Region':{leaves}] = value * 2;",
            "['Region': @'Type' = 'Coastal', 'M':'Sales'] = 100;",
            "['M':'x'] = IF value >= 10 OR value < 0 THEN value / 2 ELSE Undef();",
            "['M':'x'] = 'FX'!['Cur':'USD'] with ('Region'->'Country');",
            "['M':{children of 'Total'}] = Attr('M','Code');",
            "['M':'x'] = -(value + 1) * value['M': !'alias'];",
        ];
        for src in corpus {
            let once = parse(src).unwrap().to_string();
            let twice = parse(&once)
                .unwrap_or_else(|e| panic!("re-parse of `{once}` failed: {e}"))
                .to_string();
            assert_eq!(once, twice, "round-trip not stable for `{src}`");
        }
    }

    #[test]
    fn parse_is_deterministic() {
        let src = "['M':'x'] = IF value > 0 THEN value * 'FX'!['C':'USD'] ELSE 0;";
        assert_eq!(canon(src), canon(src));
    }

    #[test]
    fn property_random_documents_round_trip() {
        // Build random multi-rule documents from a fragment corpus and assert
        // parse success, rule count, and Display idempotence (deterministic).
        let fragments = [
            "['M':'a'] = value + 1;",
            "['Region':{leaves}, 'M':'b'] = value['M':'Sales'] * 2;",
            "['M':'c'] = IF value > 0 THEN value ELSE 0;",
            "['Region': @'Type' = 'X'] = Attr('Region','Code');",
            "['M':'d'] = -value / (value + 1);",
        ];
        let mut rng = DeterministicRng::new(0xC0FFEE);
        for _ in 0..200 {
            let count = (rng.next_u64() % 5) as usize + 1;
            let mut src = String::new();
            for _ in 0..count {
                let pick = (rng.next_u64() as usize) % fragments.len();
                src.push_str(fragments[pick]);
                src.push('\n');
            }
            let doc = parse(&src).unwrap();
            assert_eq!(doc.rules.len(), count);
            let once = doc.to_string();
            assert_eq!(once, parse(&once).unwrap().to_string());
        }
    }
}
