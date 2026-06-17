//! TypeScript type stripping: turn a flow's TypeScript source into runnable
//! JavaScript by blanking the type-only constructs (the embedded engine runs
//! JavaScript, ADR-0004).
//!
//! This is a focused, dependency-free stripper in the spirit of the hand-written
//! MDX and rules lexers, not a full TypeScript compiler. Type *checking* is the
//! editor's job (a shipped `.d.ts`); the server only needs to remove types so the
//! script parses. The design is conservative and **fail-loud**: a construct it
//! cannot confidently classify is left untouched, so the worst case is a runtime
//! parse error pointing at the offending line, never silently corrupted logic.
//!
//! Stripped regions are replaced by spaces (newlines preserved), so the output is
//! the same length and line layout as the input and a parse error in the
//! JavaScript maps back to the TypeScript source line and column.
//!
//! ## Supported subset
//! - Type annotations `: Type` on variables, parameters, and function return
//!   types, where `Type` is a "simple type": an identifier (optionally
//!   dotted/qualified), with generic arguments `<...>`, array suffixes `[]`,
//!   string/number literal types, and `|`/`&` unions of those. Inline object
//!   types `{...}` and function types `(...) => ...` are not supported in
//!   annotation position (use a named `interface`/`type` alias).
//! - `interface NAME ... { ... }` and `type NAME = ... ;` declarations (dropped).
//! - `as Type` assertions (dropped).
//! - Optional markers `?:` on parameters and variables.
//! - Generic parameter lists on `function NAME<...>(`.
//! - A leading `export` on a declaration (dropped; flows declare top-level
//!   functions that the runtime calls by name).
//!
//! ## Rejected (errors)
//! `enum`, `namespace`, `declare`, `class`, decorators (`@`), and `import`
//! statements: these need real compilation or a module loader the flow sandbox
//! does not provide.

use std::fmt;

/// A type-stripping failure: an unsupported construct, with its location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripError {
    /// What went wrong.
    pub message: String,
    /// 1-based line of the offending construct.
    pub line: usize,
    /// 1-based column of the offending construct.
    pub column: usize,
}

impl fmt::Display for StripError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (line {}, column {})",
            self.message, self.line, self.column
        )
    }
}

impl std::error::Error for StripError {}

/// The previous significant token category, used to classify a following `{`
/// (block vs object literal) and `/` (regex vs division).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// Start of input or after a statement separator: expression/statement start.
    Start,
    /// A token that ends a value (identifier, number, string, `)`, `]`, `}`),
    /// so a following `/` is division and a following `{` is a block.
    Value,
    /// An operator or opener that puts us in expression position, so a following
    /// `{` is an object literal and `/` is a regex.
    Op,
    /// A keyword that is followed by an expression (`return`, `typeof`, ...).
    KeywordExpr,
}

struct Stripper {
    src: Vec<char>,
    out: Vec<char>,
    i: usize,
    prev: Prev,
    /// Brace scopes: `true` = object literal, `false` = block. The implicit
    /// program scope is a block.
    braces: Vec<bool>,
    /// Pending unmatched ternary `?` per current statement (reset at `;`).
    ternary: u32,
}

/// Strip TypeScript type syntax from `src`, returning runnable JavaScript of the
/// same length and line layout. Returns an error only for explicitly unsupported
/// constructs.
pub fn strip_types(src: &str) -> Result<String, StripError> {
    let chars: Vec<char> = src.chars().collect();
    let mut s = Stripper {
        out: chars.clone(),
        src: chars,
        i: 0,
        prev: Prev::Start,
        braces: Vec::new(),
        ternary: 0,
    };
    s.run()?;
    Ok(s.out.into_iter().collect())
}

impl Stripper {
    fn run(&mut self) -> Result<(), StripError> {
        while self.i < self.src.len() {
            let c = self.src[self.i];
            match c {
                c if c.is_whitespace() => {
                    self.i += 1;
                }
                '/' if self.peek(1) == Some('/') => self.skip_line_comment(),
                '/' if self.peek(1) == Some('*') => self.skip_block_comment(),
                '/' if self.prev != Prev::Value => self.skip_regex(),
                '\'' | '"' => self.skip_string(c),
                '`' => self.skip_template(),
                '{' => {
                    // Object literal in expression position, else a block.
                    let is_object = matches!(self.prev, Prev::Op | Prev::KeywordExpr);
                    self.braces.push(is_object);
                    self.prev = Prev::Op;
                    self.i += 1;
                }
                '}' => {
                    self.braces.pop();
                    // A closing brace ends a value (object/array) or a block.
                    self.prev = Prev::Value;
                    self.ternary = 0;
                    self.i += 1;
                }
                ';' => {
                    self.ternary = 0;
                    self.prev = Prev::Start;
                    self.i += 1;
                }
                '?' => self.handle_question(),
                ':' => self.handle_colon(),
                'a'..='z' | 'A'..='Z' | '_' | '$' => self.handle_word()?,
                '0'..='9' => {
                    self.read_number();
                    self.prev = Prev::Value;
                }
                ')' | ']' => {
                    self.prev = Prev::Value;
                    self.i += 1;
                }
                '(' | '[' | ',' => {
                    self.prev = Prev::Op;
                    self.i += 1;
                }
                '@' => {
                    return Err(self.error_at(self.i, "decorators are not supported in flows"));
                }
                _ => {
                    // Any other operator/punctuation puts us in expression
                    // position (so a following `{` is an object, `/` is a regex).
                    self.prev = Prev::Op;
                    self.i += 1;
                }
            }
        }
        Ok(())
    }

    fn peek(&self, n: usize) -> Option<char> {
        self.src.get(self.i + n).copied()
    }

    /// Blank `[start, end)` to spaces, preserving newlines (keeps line layout).
    fn blank(&mut self, start: usize, end: usize) {
        for k in start..end {
            if self.out[k] != '\n' && self.out[k] != '\r' {
                self.out[k] = ' ';
            }
        }
    }

    fn error_at(&self, offset: usize, message: &str) -> StripError {
        let (mut line, mut column) = (1usize, 1usize);
        for &ch in &self.src[..offset.min(self.src.len())] {
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        StripError {
            message: message.to_string(),
            line,
            column,
        }
    }

    fn skip_line_comment(&mut self) {
        while self.i < self.src.len() && self.src[self.i] != '\n' {
            self.i += 1;
        }
    }

    fn skip_block_comment(&mut self) {
        self.i += 2;
        while self.i < self.src.len() {
            if self.src[self.i] == '*' && self.peek(1) == Some('/') {
                self.i += 2;
                return;
            }
            self.i += 1;
        }
    }

    fn skip_string(&mut self, quote: char) {
        self.i += 1;
        while self.i < self.src.len() {
            let c = self.src[self.i];
            if c == '\\' {
                self.i += 2;
                continue;
            }
            self.i += 1;
            if c == quote {
                break;
            }
        }
        self.prev = Prev::Value;
    }

    fn skip_template(&mut self) {
        self.i += 1;
        let mut depth = 0usize;
        while self.i < self.src.len() {
            let c = self.src[self.i];
            if c == '\\' {
                self.i += 2;
                continue;
            }
            if depth == 0 && c == '`' {
                self.i += 1;
                break;
            }
            if c == '$' && self.peek(1) == Some('{') {
                depth += 1;
                self.i += 2;
                continue;
            }
            if depth > 0 && c == '}' {
                depth -= 1;
            }
            self.i += 1;
        }
        self.prev = Prev::Value;
    }

    fn skip_regex(&mut self) {
        self.i += 1; // opening '/'
        let mut in_class = false;
        while self.i < self.src.len() {
            let c = self.src[self.i];
            if c == '\\' {
                self.i += 2;
                continue;
            }
            match c {
                '[' => in_class = true,
                ']' => in_class = false,
                '/' if !in_class => {
                    self.i += 1;
                    break;
                }
                '\n' => break, // not a regex after all; bail
                _ => {}
            }
            self.i += 1;
        }
        // Flags.
        while self.i < self.src.len() && self.src[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        self.prev = Prev::Value;
    }

    fn read_number(&mut self) {
        while self.i < self.src.len() {
            let c = self.src[self.i];
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn read_word(&mut self) -> String {
        let start = self.i;
        while self.i < self.src.len() {
            let c = self.src[self.i];
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                self.i += 1;
            } else {
                break;
            }
        }
        self.src[start..self.i].iter().collect()
    }

    fn skip_ws_and_comments(&mut self) {
        self.i = self.skip_trivia(self.i);
    }

    /// Skip whitespace and comments starting at `k`, returning the next
    /// significant offset. Pure (does not move `self.i`).
    fn skip_trivia(&self, mut k: usize) -> usize {
        loop {
            while k < self.src.len() && self.src[k].is_whitespace() {
                k += 1;
            }
            if self.src.get(k) == Some(&'/') && self.src.get(k + 1) == Some(&'/') {
                while k < self.src.len() && self.src[k] != '\n' {
                    k += 1;
                }
            } else if self.src.get(k) == Some(&'/') && self.src.get(k + 1) == Some(&'*') {
                k += 2;
                while k < self.src.len()
                    && !(self.src[k] == '*' && self.src.get(k + 1) == Some(&'/'))
                {
                    k += 1;
                }
                k = (k + 2).min(self.src.len());
            } else {
                break;
            }
        }
        k
    }

    /// Whether the token before position `at` (skipping spaces backward) is a `.`,
    /// which would make a keyword actually a property access (`obj.type`).
    fn prev_is_dot(&self, at: usize) -> bool {
        let mut k = at;
        while k > 0 {
            k -= 1;
            let c = self.src[k];
            if c.is_whitespace() {
                continue;
            }
            return c == '.';
        }
        false
    }

    fn handle_word(&mut self) -> Result<(), StripError> {
        let start = self.i;
        let word = self.read_word();
        let after_dot = self.prev_is_dot(start);

        if !after_dot {
            match word.as_str() {
                "enum" | "namespace" | "declare" | "class" => {
                    return Err(
                        self.error_at(start, &format!("'{word}' is not supported in flows"))
                    );
                }
                "import" => {
                    return Err(self.error_at(
                        start,
                        "import is not supported in flows (the host API is the global 'ctx')",
                    ));
                }
                "interface" => {
                    self.strip_interface(start);
                    self.prev = Prev::Start;
                    return Ok(());
                }
                "type" if self.looks_like_type_alias() => {
                    self.strip_type_alias(start)?;
                    self.prev = Prev::Start;
                    return Ok(());
                }
                "export" => {
                    // Drop the `export` keyword; flows declare top-level functions
                    // the runtime calls by name. `export type`/`export interface`
                    // are handled when their keyword is read next.
                    self.blank(start, self.i);
                    self.prev = Prev::Start;
                    return Ok(());
                }
                "as" if self.prev == Prev::Value => {
                    self.strip_as(start);
                    self.prev = Prev::Value;
                    return Ok(());
                }
                "function" => {
                    self.prev = Prev::KeywordExpr;
                    self.maybe_strip_function_generics();
                    return Ok(());
                }
                "return" | "typeof" | "instanceof" | "in" | "of" | "new" | "delete" | "void"
                | "yield" | "await" | "case" | "do" | "else" => {
                    self.prev = Prev::KeywordExpr;
                    return Ok(());
                }
                _ => {}
            }
        }
        self.prev = Prev::Value;
        Ok(())
    }

    /// Heuristic: `type` is a type-alias keyword when followed by an identifier
    /// and then `=` or `<` (`type X = ...`, `type X<T> = ...`). Otherwise it is an
    /// ordinary identifier (e.g. a variable named `type`).
    fn looks_like_type_alias(&self) -> bool {
        let mut k = self.i;
        while k < self.src.len() && self.src[k].is_whitespace() {
            k += 1;
        }
        // identifier
        let id_start = k;
        while k < self.src.len()
            && (self.src[k].is_ascii_alphanumeric() || self.src[k] == '_' || self.src[k] == '$')
        {
            k += 1;
        }
        if k == id_start {
            return false;
        }
        while k < self.src.len() && self.src[k].is_whitespace() {
            k += 1;
        }
        matches!(self.src.get(k), Some('=') | Some('<'))
    }

    fn strip_interface(&mut self, start: usize) {
        // Advance to the opening brace, then balance braces.
        while self.i < self.src.len() && self.src[self.i] != '{' {
            self.i += 1;
        }
        if self.i >= self.src.len() {
            self.blank(start, self.i);
            return;
        }
        let mut depth = 0usize;
        while self.i < self.src.len() {
            match self.src[self.i] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    self.i += 1;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            self.i += 1;
        }
        self.blank(start, self.i);
    }

    fn strip_type_alias(&mut self, start: usize) -> Result<(), StripError> {
        // `type X ... = ... ;` - balance brackets, stop at the top-level `;` (or
        // end of a line with no nesting, as a fallback).
        let mut depth: i32 = 0;
        while self.i < self.src.len() {
            match self.src[self.i] {
                '{' | '(' | '[' | '<' => depth += 1,
                '}' | ')' | ']' | '>' => depth -= 1,
                ';' if depth <= 0 => {
                    self.i += 1;
                    self.blank(start, self.i);
                    return Ok(());
                }
                '\n' if depth <= 0 => {
                    // Allow semicolon-less aliases terminated by a newline.
                    self.blank(start, self.i);
                    return Ok(());
                }
                _ => {}
            }
            self.i += 1;
        }
        self.blank(start, self.i);
        Ok(())
    }

    fn strip_as(&mut self, start: usize) {
        // `as` keyword already consumed (self.i past it). Skip the following
        // simple type; blank from `as` through the type.
        let after_as = self.i;
        self.skip_ws_and_comments();
        if let Some(end) = self.scan_simple_type() {
            self.blank(start, end);
            self.i = end;
        } else {
            // Not a recognizable type; leave `as` alone (likely an identifier).
            self.i = after_as;
            self.prev = Prev::Value;
        }
    }

    fn maybe_strip_function_generics(&mut self) {
        // After `function`, optionally an identifier, then optional `<...>`.
        let save = self.i;
        self.skip_ws_and_comments();
        // function name (optional for anonymous)
        if self
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        {
            let _ = self.read_word();
        }
        self.skip_ws_and_comments();
        if self.peek(0) == Some('<') {
            let gen_start = self.i;
            if let Some(end) = self.scan_angle_balanced() {
                self.blank(gen_start, end);
                self.i = end;
                self.prev = Prev::Value;
                return;
            }
        }
        // No generics to strip; rewind so the name/params parse normally.
        self.i = save;
    }

    fn handle_question(&mut self) {
        // `?.` optional chaining, `??` nullish: operators, leave alone.
        if matches!(self.peek(1), Some('.') | Some('?')) {
            self.prev = Prev::Op;
            self.i += 2;
            return;
        }
        // `?:` optional marker (parameter/property): blank the `?`, let the colon
        // be handled as an annotation next.
        let mut k = self.i + 1;
        while k < self.src.len() && self.src[k].is_whitespace() {
            k += 1;
        }
        if self.src.get(k) == Some(&':') {
            self.blank(self.i, self.i + 1);
            self.i += 1;
            self.prev = Prev::Op;
            return;
        }
        // Ternary.
        self.ternary += 1;
        self.prev = Prev::Op;
        self.i += 1;
    }

    fn handle_colon(&mut self) {
        let colon = self.i;
        // Ternary `:` pairs with a pending `?`.
        if self.ternary > 0 {
            self.ternary -= 1;
            self.prev = Prev::Op;
            self.i += 1;
            return;
        }
        // Object-literal key separator: keep.
        if self.braces.last() == Some(&true) {
            self.prev = Prev::Op;
            self.i += 1;
            return;
        }
        // Otherwise a candidate type annotation (block/param/return position).
        self.i += 1;
        self.skip_ws_and_comments();
        let type_start = self.i;
        if let Some(end) = self.scan_simple_type() {
            // Only strip when a plausible terminator follows, so a label
            // (`outer: for`) or other non-annotation colon is left intact.
            if self.is_annotation_terminator(end) {
                self.blank(colon, end);
                self.i = end;
                // The annotation is transparent: the position is whatever the
                // name/`)` before the colon left it (a value), so a following `{`
                // (a function body after `): T {`) is a block, not an object.
                self.prev = Prev::Value;
                return;
            }
        }
        // Not a recognizable annotation; leave the colon (fail loud at parse time
        // rather than risk corrupting code).
        self.i = type_start;
        self.prev = Prev::Op;
    }

    /// Whether what follows a candidate annotation type (at offset `end`) is a
    /// token that legitimately ends an annotation: `, ) ; = { => end`, or the
    /// `of`/`in` of a typed `for` binding.
    fn is_annotation_terminator(&self, end: usize) -> bool {
        let k = self.skip_trivia(end);
        match self.src.get(k) {
            None | Some(',') | Some(')') | Some(';') | Some('=') | Some('{') => true,
            Some(c) if c.is_ascii_alphabetic() => {
                let mut j = k;
                while j < self.src.len()
                    && (self.src[j].is_ascii_alphanumeric()
                        || self.src[j] == '_'
                        || self.src[j] == '$')
                {
                    j += 1;
                }
                let word: String = self.src[k..j].iter().collect();
                word == "of" || word == "in"
            }
            _ => false,
        }
    }

    /// Scan a simple type starting at `self.i`, returning the offset just past it
    /// (without consuming), or `None` if it is not a recognizable simple type.
    /// Grammar: `union := primary (('|'|'&') primary)*`,
    /// `primary := name ('.' name)* angle? array*  |  string  |  number`.
    fn scan_simple_type(&self) -> Option<usize> {
        let mut k = self.i;
        loop {
            k = self.scan_type_primary(k)?;
            // Union/intersection continuation.
            let mut j = k;
            while j < self.src.len() && self.src[j].is_whitespace() {
                j += 1;
            }
            if matches!(self.src.get(j), Some('|') | Some('&')) {
                // Not `||`/`&&` (those are value operators, shouldn't appear here).
                k = j + 1;
                while k < self.src.len() && self.src[k].is_whitespace() {
                    k += 1;
                }
                continue;
            }
            return Some(k);
        }
    }

    fn scan_type_primary(&self, mut k: usize) -> Option<usize> {
        while k < self.src.len() && self.src[k].is_whitespace() {
            k += 1;
        }
        match self.src.get(k)? {
            // String literal type.
            '\'' | '"' => {
                let quote = self.src[k];
                k += 1;
                while k < self.src.len() && self.src[k] != quote {
                    if self.src[k] == '\\' {
                        k += 1;
                    }
                    k += 1;
                }
                k += 1; // closing quote
            }
            // Numeric literal type.
            '0'..='9' => {
                while k < self.src.len()
                    && (self.src[k].is_ascii_alphanumeric() || self.src[k] == '.')
                {
                    k += 1;
                }
            }
            // Qualified name with optional generic args.
            c if c.is_ascii_alphabetic() || *c == '_' || *c == '$' => {
                loop {
                    // identifier
                    let id_start = k;
                    while k < self.src.len()
                        && (self.src[k].is_ascii_alphanumeric()
                            || self.src[k] == '_'
                            || self.src[k] == '$')
                    {
                        k += 1;
                    }
                    if k == id_start {
                        return None;
                    }
                    // dotted continuation
                    let mut j = k;
                    while j < self.src.len() && self.src[j].is_whitespace() {
                        j += 1;
                    }
                    if self.src.get(j) == Some(&'.') {
                        k = j + 1;
                        while k < self.src.len() && self.src[k].is_whitespace() {
                            k += 1;
                        }
                        continue;
                    }
                    break;
                }
                // optional generic args
                let mut j = k;
                while j < self.src.len() && self.src[j].is_whitespace() {
                    j += 1;
                }
                if self.src.get(j) == Some(&'<') {
                    k = self.scan_angle_balanced_from(j)?;
                }
            }
            _ => return None,
        }
        // array suffixes `[]`
        loop {
            let mut j = k;
            while j < self.src.len() && self.src[j].is_whitespace() {
                j += 1;
            }
            if self.src.get(j) == Some(&'[') && self.src.get(j + 1) == Some(&']') {
                k = j + 2;
            } else {
                break;
            }
        }
        Some(k)
    }

    /// Balance a `<...>` starting at `self.i`.
    fn scan_angle_balanced(&self) -> Option<usize> {
        self.scan_angle_balanced_from(self.i)
    }

    /// Balance a `<...>` starting at `from` (which must point at `<`), returning
    /// the offset just past the closing `>`. Tracks nested `<>` and the `[]`
    /// inside; bails on tokens that cannot appear in a type-argument list.
    fn scan_angle_balanced_from(&self, from: usize) -> Option<usize> {
        let mut k = from;
        let mut depth = 0i32;
        while k < self.src.len() {
            match self.src[k] {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(k + 1);
                    }
                }
                // Tokens that never appear inside a simple type-argument list.
                ';' | '{' | '}' | '(' | ')' => return None,
                _ => {}
            }
            k += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip, then remove all whitespace, so tests assert the surviving tokens
    /// while the stripper keeps exact positions (it only blanks type regions and
    /// never deletes inter-token spacing, so whitespace-insensitive comparison is
    /// sound). The expected strings are written readably and normalized the same
    /// way.
    fn nows(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }
    fn js(src: &str) -> String {
        nows(&strip_types(src).unwrap())
    }
    #[track_caller]
    fn check(src: &str, expected: &str) {
        assert_eq!(js(src), nows(expected));
    }

    #[test]
    fn keeps_length_and_lines() {
        let src = "let x: number = 1;\nlet y: string = 'a';\n";
        let out = strip_types(src).unwrap();
        assert_eq!(out.len(), src.len(), "output preserves byte length");
        assert_eq!(out.lines().count(), src.lines().count());
    }

    #[test]
    fn strips_variable_annotations() {
        check("let x: number = 1;", "let x = 1;");
        check("const s: string = 'hi';", "const s = 'hi';");
        check("let xs: string[] = [];", "let xs = [];");
        check(
            "let m: Map<string, number> = new Map();",
            "let m = new Map();",
        );
    }

    #[test]
    fn strips_function_signatures() {
        check(
            "function rows(ctx: FlowContext): void { return; }",
            "function rows(ctx) { return; }",
        );
        check(
            "function add(a: number, b: number): number { return a + b; }",
            "function add(a, b) { return a + b; }",
        );
    }

    #[test]
    fn strips_optional_params() {
        check(
            "function f(a?: number) { return a; }",
            "function f(a) { return a; }",
        );
    }

    #[test]
    fn strips_function_generics() {
        check(
            "function id<T>(x: T): T { return x; }",
            "function id(x) { return x; }",
        );
    }

    #[test]
    fn strips_interface_and_type_alias() {
        check(
            "interface Row { region: string; value: number }\nlet r = 1;",
            "let r = 1;",
        );
        check("type Id = string;\nlet x = 2;", "let x = 2;");
        check(
            "type Pair = { a: number; b: number };\nlet y = 3;",
            "let y = 3;",
        );
    }

    #[test]
    fn strips_as_casts() {
        check("const n = x as number;", "const n = x;");
        check("const r = obj as Row;", "const r = obj;");
    }

    #[test]
    fn strips_export_keyword() {
        check(
            "export function rows(ctx: Ctx): void {}",
            "function rows(ctx) {}",
        );
    }

    #[test]
    fn preserves_object_literals() {
        // The colon in an object literal value is NOT an annotation.
        check("const o = { a: 1, b: 2 };", "const o = { a: 1, b: 2 };");
        check(
            "return { region: r, value: v };",
            "return { region: r, value: v };",
        );
        // Object value that is itself a bare identifier (the dangerous case).
        check(
            "const o = { status: string };",
            "const o = { status: string };",
        );
    }

    #[test]
    fn preserves_ternary() {
        check("const x = c ? a : b;", "const x = c ? a : b;");
        check(
            "const x = c ? { a: 1 } : { b: 2 };",
            "const x = c ? { a: 1 } : { b: 2 };",
        );
    }

    #[test]
    fn preserves_division_and_strings_and_comments() {
        check("const r = a / b / c;", "const r = a / b / c;");
        // A ':' inside a string is untouched.
        check("const s = 'a: number';", "const s = 'a: number';");
        // A ':' inside a comment is untouched.
        let out = strip_types("// a: number\nlet x = 1;").unwrap();
        assert!(out.contains("// a: number"));
    }

    #[test]
    fn preserves_template_literals() {
        check("const s = `x:${v}`;", "const s = `x:${v}`;");
    }

    #[test]
    fn preserves_labels() {
        // A label colon must not be stripped (the following token is not a type).
        check(
            "outer: for (let i = 0; i < 3; i++) { break outer; }",
            "outer: for (let i = 0; i < 3; i++) { break outer; }",
        );
    }

    #[test]
    fn rejects_unsupported_constructs() {
        assert!(strip_types("enum E { A, B }").is_err());
        assert!(strip_types("namespace N {}").is_err());
        assert!(strip_types("import { x } from 'y';").is_err());
        assert!(strip_types("@dec class C {}").is_err());
        assert!(strip_types("class C { x: number }").is_err());
        assert!(strip_types("export class C {}").is_err());
    }

    #[test]
    fn strips_annotation_with_comment_before_terminator() {
        // A comment between the type and its terminator must not block stripping;
        // the type goes, the comment stays.
        let out = strip_types("const x: number // tag\n = 5;").unwrap();
        assert!(!out.contains("number"), "annotation stripped: {out:?}");
        assert!(out.contains("// tag"), "comment preserved: {out:?}");
        assert!(out.contains("= 5"));

        let out = strip_types("function f(a: string /* c */) { return a; }").unwrap();
        assert!(!out.contains("string"), "annotation stripped: {out:?}");
        assert!(out.contains("/* c */"), "comment preserved: {out:?}");
        assert!(out.contains("return a"));
    }

    #[test]
    fn error_carries_location() {
        let err = strip_types("let x = 1;\nenum E { A }").unwrap_err();
        assert_eq!(err.line, 2);
    }

    #[test]
    fn union_and_qualified_types() {
        check("let x: A | B = a;", "let x = a;");
        check("let x: ns.Type = a;", "let x = a;");
        check(
            "function f(): A.B<C> { return x; }",
            "function f() { return x; }",
        );
    }

    #[test]
    fn realistic_flow_strips_to_valid_js_shape() {
        let src = "\
export function rows(ctx: FlowContext): void {
  const rows: Row[] = ctx.input();
  for (const r of rows) {
    const region: string = r.Region;
    const value: number = Number(r.Value);
    ctx.ensureElements('Region', [region]);
    ctx.writeCells([{ coord: { Region: region }, value: value }]);
  }
}";
        // `js` removes whitespace, so assert on whitespace-free token runs.
        let out = js(src);
        assert!(!out.contains(":void"));
        assert!(!out.contains(":Row[]"));
        assert!(!out.contains(":string"));
        assert!(!out.contains(":number"));
        // Object literals inside writeCells survive (colon-bearing keys kept).
        assert!(out.contains("coord:{Region:region}"), "{out}");
        assert!(out.contains("value:value"));
        assert!(out.contains("functionrows(ctx){"));
    }
}
