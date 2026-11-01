//! The `rules:if` / `only:variables` expression language.
//!
//! ```text
//! or      := and ('||' and)*
//! and     := unary ('&&' unary)*
//! unary   := '!' unary | primary
//! primary := '(' or ')' | operand (('==' | '!=' | '=~' | '!~') operand)?
//! operand := $VAR | ${VAR} | "str" | 'str' | null | /regex/flags
//! ```
//!
//! A lone operand is true when it is a defined, non-empty value. Undefined
//! variables are `null`.

use std::collections::BTreeMap;

use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Var(String),
    Str(String),
    Null,
    Pattern { source: String, flags: String },
    Eq,
    Ne,
    Match,
    NoMatch,
    And,
    Or,
    Not,
    LParen,
    RParen,
}

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Var(String),
    Str(String),
    Null,
    Pattern { source: String, flags: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Match,
    NoMatch,
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Truthy(Operand),
    Cmp(Operand, CmpOp, Operand),
}

/// Evaluates `src` against `vars`. The error is a human-readable reason.
pub fn evaluate(src: &str, vars: &BTreeMap<String, String>) -> Result<bool, String> {
    let tokens = lex(src)?;
    if tokens.is_empty() {
        return Err("empty expression".into());
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.or()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!("unexpected {}", describe(&parser.tokens[parser.pos])));
    }
    eval(&expr, vars)
}

fn describe(t: &Token) -> String {
    match t {
        Token::Var(v) => format!("variable `${v}`"),
        Token::Str(s) => format!("string \"{s}\""),
        Token::Null => "`null`".into(),
        Token::Pattern { source, .. } => format!("pattern /{source}/"),
        Token::Eq => "`==`".into(),
        Token::Ne => "`!=`".into(),
        Token::Match => "`=~`".into(),
        Token::NoMatch => "`!~`".into(),
        Token::And => "`&&`".into(),
        Token::Or => "`||`".into(),
        Token::Not => "`!`".into(),
        Token::LParen => "`(`".into(),
        Token::RParen => "`)`".into(),
    }
}

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// True when the previous token ends an operand (then `/` cannot open a pattern).
fn after_operand(tokens: &[Token]) -> bool {
    matches!(tokens.last(), Some(Token::Var(_) | Token::Str(_) | Token::Null | Token::Pattern { .. } | Token::RParen))
}

fn lex(src: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '=' if next == Some('=') => {
                tokens.push(Token::Eq);
                i += 2;
            }
            '=' if next == Some('~') => {
                tokens.push(Token::Match);
                i += 2;
            }
            '!' if next == Some('=') => {
                tokens.push(Token::Ne);
                i += 2;
            }
            '!' if next == Some('~') => {
                tokens.push(Token::NoMatch);
                i += 2;
            }
            '!' => {
                tokens.push(Token::Not);
                i += 1;
            }
            '&' if next == Some('&') => {
                tokens.push(Token::And);
                i += 2;
            }
            '|' if next == Some('|') => {
                tokens.push(Token::Or);
                i += 2;
            }
            '$' => {
                let (name, end) = if next == Some('{') {
                    let start = i + 2;
                    let mut j = start;
                    while j < chars.len() && is_var_char(chars[j]) {
                        j += 1;
                    }
                    if chars.get(j) != Some(&'}') {
                        return Err("unterminated `${`".into());
                    }
                    (chars[start..j].iter().collect::<String>(), j + 1)
                } else {
                    let start = i + 1;
                    let mut j = start;
                    while j < chars.len() && is_var_char(chars[j]) {
                        j += 1;
                    }
                    (chars[start..j].iter().collect::<String>(), j)
                };
                if name.is_empty() {
                    return Err("`$` without a variable name".into());
                }
                tokens.push(Token::Var(name));
                i = end;
            }
            '"' | '\'' => {
                let quote = c;
                let mut out = String::new();
                let mut j = i + 1;
                loop {
                    match chars.get(j) {
                        None => return Err("unterminated string".into()),
                        Some(&ch) if ch == quote => break,
                        Some('\\') => {
                            match chars.get(j + 1) {
                                Some(&e) if e == quote || e == '\\' => out.push(e),
                                Some(&e) => {
                                    out.push('\\');
                                    out.push(e);
                                }
                                None => return Err("unterminated string".into()),
                            }
                            j += 2;
                        }
                        Some(&ch) => {
                            out.push(ch);
                            j += 1;
                        }
                    }
                }
                tokens.push(Token::Str(out));
                i = j + 1;
            }
            '/' if !after_operand(&tokens) => {
                let mut source = String::new();
                let mut j = i + 1;
                loop {
                    match chars.get(j) {
                        None => return Err("unterminated pattern".into()),
                        Some('/') => break,
                        Some('\\') if chars.get(j + 1) == Some(&'/') => {
                            source.push('/');
                            j += 2;
                        }
                        Some('\\') => {
                            source.push('\\');
                            if let Some(&e) = chars.get(j + 1) {
                                source.push(e);
                            }
                            j += 2;
                        }
                        Some(&ch) => {
                            source.push(ch);
                            j += 1;
                        }
                    }
                }
                j += 1;
                let mut flags = String::new();
                while let Some(&f) = chars.get(j) {
                    if !f.is_ascii_alphabetic() {
                        break;
                    }
                    flags.push(f);
                    j += 1;
                }
                tokens.push(Token::Pattern { source, flags });
                i = j;
            }
            c if c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && is_var_char(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if word == "null" {
                    tokens.push(Token::Null);
                } else {
                    return Err(format!("unexpected word `{word}` (strings need quotes)"));
                }
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.peek() == Some(&Token::Or) {
            self.pos += 1;
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while self.peek() == Some(&Token::And) {
            self.pos += 1;
            let right = self.unary()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(&Token::Not) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Token::LParen) => {
                self.pos += 1;
                let inner = self.or()?;
                if self.peek() != Some(&Token::RParen) {
                    return Err("missing `)`".into());
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(_) => {
                let left = self.operand()?;
                let op = match self.peek() {
                    Some(Token::Eq) => CmpOp::Eq,
                    Some(Token::Ne) => CmpOp::Ne,
                    Some(Token::Match) => CmpOp::Match,
                    Some(Token::NoMatch) => CmpOp::NoMatch,
                    _ => return Ok(Expr::Truthy(left)),
                };
                self.pos += 1;
                let right = self.operand()?;
                Ok(Expr::Cmp(left, op, right))
            }
            None => Err("unexpected end of expression".into()),
        }
    }

    fn operand(&mut self) -> Result<Operand, String> {
        let token = self.peek().cloned().ok_or("unexpected end of expression")?;
        let operand = match token {
            Token::Var(v) => Operand::Var(v),
            Token::Str(s) => Operand::Str(s),
            Token::Null => Operand::Null,
            Token::Pattern { source, flags } => {
                // Caught at parse time, so a bad pattern fails even when unused.
                compile(&source, &flags)?;
                Operand::Pattern { source, flags }
            }
            other => return Err(format!("expected a value, found {}", describe(&other))),
        };
        self.pos += 1;
        Ok(operand)
    }
}

#[derive(Debug)]
enum Val {
    Null,
    Str(String),
    Pattern { source: String, flags: String },
}

fn resolve(op: &Operand, vars: &BTreeMap<String, String>) -> Val {
    match op {
        Operand::Var(name) => vars.get(name).map_or(Val::Null, |v| Val::Str(v.clone())),
        Operand::Str(s) => Val::Str(s.clone()),
        Operand::Null => Val::Null,
        Operand::Pattern { source, flags } => Val::Pattern { source: source.clone(), flags: flags.clone() },
    }
}

fn eval(expr: &Expr, vars: &BTreeMap<String, String>) -> Result<bool, String> {
    Ok(match expr {
        Expr::Or(a, b) => eval(a, vars)? || eval(b, vars)?,
        Expr::And(a, b) => eval(a, vars)? && eval(b, vars)?,
        Expr::Not(a) => !eval(a, vars)?,
        Expr::Truthy(op) => match resolve(op, vars) {
            Val::Null => false,
            Val::Str(s) => !s.is_empty(),
            Val::Pattern { .. } => true,
        },
        Expr::Cmp(l, op, r) => {
            let (l, r) = (resolve(l, vars), resolve(r, vars));
            match op {
                CmpOp::Eq => equal(&l, &r),
                CmpOp::Ne => !equal(&l, &r),
                CmpOp::Match => matches(&l, &r)?,
                CmpOp::NoMatch => !matches(&l, &r)?,
            }
        }
    })
}

fn as_text(v: &Val) -> Option<String> {
    match v {
        Val::Null => None,
        Val::Str(s) => Some(s.clone()),
        Val::Pattern { source, flags } => Some(format!("/{source}/{flags}")),
    }
}

fn equal(l: &Val, r: &Val) -> bool {
    as_text(l) == as_text(r)
}

fn matches(l: &Val, r: &Val) -> Result<bool, String> {
    let subject = match l {
        Val::Str(s) => s,
        // Undefined (or a pattern) on the left never matches.
        _ => return Ok(false),
    };
    let regex = match r {
        Val::Null => return Ok(false),
        Val::Pattern { source, flags } => compile(source, flags)?,
        // A variable (or string) holding a pattern: `/re/flags`, else the text itself.
        Val::Str(s) => match split_pattern(s) {
            Some((source, flags)) => compile(&source, &flags)?,
            None => compile(s, "")?,
        },
    };
    Ok(regex.is_match(subject))
}

/// `only: [/re/flags]`: does `subject` match the pattern literal?
pub fn pattern_matches(literal: &str, subject: &str) -> Result<bool, String> {
    let (source, flags) = split_pattern(literal).ok_or_else(|| format!("`{literal}` is not a /pattern/"))?;
    Ok(compile(&source, &flags)?.is_match(subject))
}

/// `/re/flags` → (`re`, `flags`).
fn split_pattern(s: &str) -> Option<(String, String)> {
    let rest = s.strip_prefix('/')?;
    let end = rest.rfind('/')?;
    let flags = &rest[end + 1..];
    if !flags.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((rest[..end].replace("\\/", "/"), flags.to_string()))
}

fn compile(source: &str, flags: &str) -> Result<Regex, String> {
    let mut builder = RegexBuilder::new(source);
    for f in flags.chars() {
        match f {
            'i' => builder.case_insensitive(true),
            'm' => builder.multi_line(true),
            's' => builder.dot_matches_new_line(true),
            'x' => builder.ignore_whitespace(true),
            other => return Err(format!("unsupported pattern flag `{other}`")),
        };
    }
    builder.build().map_err(|e| format!("invalid pattern /{source}/: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn ev(src: &str, pairs: &[(&str, &str)]) -> bool {
        evaluate(src, &vars(pairs)).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    #[test]
    fn equality_and_null() {
        let v = [("A", "main"), ("EMPTY", "")];
        assert!(ev(r#"$A == "main""#, &v));
        assert!(ev(r#"$A == 'main'"#, &v));
        assert!(ev(r#"${A} == "main""#, &v));
        assert!(ev(r#""main" == $A"#, &v));
        assert!(!ev(r#"$A != "main""#, &v));
        assert!(ev("$MISSING == null", &v));
        assert!(!ev("$EMPTY == null", &v));
        assert!(ev(r#"$EMPTY == """#, &v));
        assert!(!ev(r#"$MISSING == """#, &v));
        assert!(ev("$A != null", &v));
        assert!(ev("null == null", &v));
    }

    #[test]
    fn truthiness() {
        let v = [("A", "x"), ("EMPTY", "")];
        assert!(ev("$A", &v));
        assert!(!ev("$EMPTY", &v));
        assert!(!ev("$MISSING", &v));
        assert!(ev("!$MISSING", &v));
        assert!(ev("!!$A", &v));
    }

    #[test]
    fn var_to_var() {
        let v = [("A", "main"), ("B", "main"), ("C", "dev")];
        assert!(ev("$A == $B", &v));
        assert!(ev("$A != $C", &v));
        assert!(!ev("$A == $MISSING", &v));
    }

    #[test]
    fn precedence_and_parens() {
        let v = [("A", "1")];
        // && binds tighter: true || (false && false)
        assert!(ev(r#"$A == "1" || $A == "2" && $A == "3""#, &v));
        assert!(!ev(r#"($A == "1" || $A == "2") && $A == "3""#, &v));
        assert!(ev(r#"!($A == "2")"#, &v));
        assert!(ev(r#"((($A == "1")))"#, &v));
    }

    #[test]
    fn patterns() {
        let v = [("BRANCH", "release/1.2"), ("UPPER", "MAIN"), ("PAT", "/^release\\//"), ("PLAIN", "^rel")];
        assert!(ev(r"$BRANCH =~ /^release\/\d+/", &v));
        assert!(!ev(r"$BRANCH !~ /^release/", &v));
        assert!(ev("$UPPER =~ /main/i", &v));
        assert!(!ev("$UPPER =~ /main/", &v));
        assert!(!ev("$MISSING =~ /x/", &v));
        assert!(ev("$MISSING !~ /x/", &v));
        assert!(ev("$BRANCH =~ $PAT", &v));
        assert!(ev("$BRANCH =~ $PLAIN", &v));
        assert!(!ev("$BRANCH =~ $MISSING", &v));
        // a slash after an operand is not a pattern start
        assert!(evaluate("$A / 2", &vars(&v)).is_err());
    }

    #[test]
    fn escapes_in_strings() {
        let v = [("Q", r#"say "hi""#), ("S", r"a\b")];
        assert!(ev(r#"$Q == "say \"hi\"""#, &v));
        assert!(ev(r#"$S == 'a\\b'"#, &v));
        assert!(ev(r#"$S == "a\b""#, &v));
    }

    #[test]
    fn real_workflow_rule() {
        let rule = r#"($CI_PIPELINE_SOURCE == "web" || $CI_PIPELINE_SOURCE == "api") && ($CI_COMMIT_BRANCH == "main" || $CI_COMMIT_BRANCH == "staging")"#;
        assert!(ev(rule, &[("CI_PIPELINE_SOURCE", "web"), ("CI_COMMIT_BRANCH", "staging")]));
        assert!(ev(rule, &[("CI_PIPELINE_SOURCE", "api"), ("CI_COMMIT_BRANCH", "main")]));
        assert!(!ev(rule, &[("CI_PIPELINE_SOURCE", "push"), ("CI_COMMIT_BRANCH", "main")]));
        assert!(!ev(rule, &[("CI_PIPELINE_SOURCE", "web"), ("CI_COMMIT_TAG", "v1")]));
    }

    #[test]
    fn syntax_errors() {
        let v = BTreeMap::new();
        for bad in [
            "",
            "$A ==",
            "($A",
            "$A == \"x",
            "$A =~ /x",
            "main == $A",
            "$A = \"x\"",
            "$A == \"x\" &&",
            "$",
            "${A",
            "$A $B",
            "$A =~ /(/",
            "$A =~ /x/q",
        ] {
            assert!(evaluate(bad, &v).is_err(), "{bad:?} should fail");
        }
    }
}
