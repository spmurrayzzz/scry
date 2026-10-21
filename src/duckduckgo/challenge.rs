use std::collections::HashMap;

use url::Url;

use super::{blocked, provider_target};
use crate::api::Error;
use crate::config::Limits;
use crate::context::RequestContext;

#[cfg(test)]
mod tests;

const MAX_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_OPERATIONS: usize = 128;
const MAX_FUNCTIONS: usize = 64;
const MAX_BYTES: usize = 65536;
const MAX_TOKENS: usize = 4096;

#[derive(Clone, Copy)]
enum Operation {
    Multiply(u64),
    Add(u64),
}

struct Parser<'a> {
    tokens: Vec<&'a str>,
    position: usize,
}

impl<'a> Parser<'a> {
    fn next(&mut self) -> Result<&'a str, Error> {
        let token = self
            .tokens
            .get(self.position)
            .copied()
            .ok_or_else(blocked)?;
        self.position += 1;
        Ok(token)
    }

    fn take(&mut self, expected: &str) -> bool {
        if self.tokens.get(self.position).copied() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &str) -> Result<(), Error> {
        if self.take(expected) {
            Ok(())
        } else {
            Err(blocked())
        }
    }

    fn sequence(&mut self, expected: &[&str]) -> Result<(), Error> {
        for token in expected {
            self.expect(token)?;
        }
        Ok(())
    }

    fn number(&mut self) -> Result<u64, Error> {
        let token = self.next()?;
        if token.len() > 16 || !token.bytes().all(|b| b.is_ascii_digit()) {
            return Err(blocked());
        }
        token
            .parse::<u64>()
            .ok()
            .filter(|n| *n <= MAX_INTEGER)
            .ok_or_else(blocked)
    }

    fn identifier(&mut self) -> Result<&'a str, Error> {
        let token = self.next()?;
        if token.len() > 64
            || !token
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || matches!(
                token,
                "jsa"
                    | "num"
                    | "DDG"
                    | "document"
                    | "let"
                    | "const"
                    | "var"
                    | "return"
                    | "function"
            )
        {
            return Err(blocked());
        }
        Ok(token)
    }

    fn quoted(&mut self) -> Result<&'a str, Error> {
        let token = self.next()?;
        if token.len() < 2 || !matches!(token.as_bytes()[0], b'\'' | b'"') {
            return Err(blocked());
        }
        Ok(&token[1..token.len() - 1])
    }

    fn scaffolding(&mut self) -> Result<(), Error> {
        loop {
            if self.take(";") {
                continue;
            }
            if self.take("'use strict'") || self.take("\"use strict\"") {
                self.expect(";")?;
            } else {
                return Ok(());
            }
        }
    }

    fn operation(&mut self) -> Result<Operation, Error> {
        self.sequence(&["=", "function", "(", "num", ")", "{"])?;
        let addition = if self.tokens.get(self.position) == Some(&"return") {
            None
        } else {
            if !matches!(self.next()?, "let" | "const" | "var") {
                return Err(blocked());
            }
            let local = self.identifier()?;
            self.sequence(&["=", "document", ".", "createElement", "("])?;
            if self.quoted()? != "div" {
                return Err(blocked());
            }
            self.sequence(&[")", ";", local, ".", "innerHTML", "="])?;
            let addition = match self.next()? {
                "`<p><div></p><p></div`" => 32,
                "`<li><div></li><li></div`" => 29,
                "`<div><div></div><div></div`" => 33,
                "`<br><div></br><br></div`" => 23,
                _ => return Err(blocked()),
            };
            self.expect(";")?;
            Some((local, addition))
        };
        self.expect("return")?;
        let mut groups = 0;
        while self.take("(") {
            groups += 1;
        }
        let operation = if let Some((local, addition)) = addition {
            if self.take("num") {
                self.sequence(&["+", local, ".", "innerHTML", ".", "length"])?;
            } else {
                self.sequence(&[local, ".", "innerHTML", ".", "length", "+", "num"])?;
            }
            Operation::Add(addition)
        } else {
            self.sequence(&["num", "*"])?;
            Operation::Multiply(self.number()?)
        };
        for _ in 0..groups {
            self.expect(")")?;
        }
        self.sequence(&[";", "}", ";"])?;
        Ok(operation)
    }
}

fn tokenize<'a>(body: &'a str, ctx: &RequestContext, limits: &Limits) -> Result<Parser<'a>, Error> {
    if body.len() > MAX_BYTES {
        return Err(blocked());
    }
    let bytes = body.as_bytes();
    let mut position = 0;
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    while position < bytes.len() {
        ctx.check()?;
        let start = position;
        let byte = bytes[position];
        position += 1;
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'/' && bytes.get(position) == Some(&b'*') {
            position += 1;
            position += body[position..].find("*/").ok_or_else(blocked)? + 2;
            continue;
        }
        if byte == b'/' && bytes.get(position) == Some(&b'/') {
            position += body[position..]
                .find(['\r', '\n', '\u{2028}', '\u{2029}'])
                .unwrap_or(bytes.len() - position);
            continue;
        }
        if tokens.len() >= MAX_TOKENS.min(limits.max_html_tokens) {
            return Err(blocked());
        }
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            while position < bytes.len()
                && (bytes[position].is_ascii_alphanumeric() || bytes[position] == b'_')
            {
                position += 1;
                if position - start > 64 {
                    return Err(blocked());
                }
            }
        } else if matches!(byte, b'\'' | b'"' | b'`') {
            while position < bytes.len() && bytes[position] != byte {
                if position % 4096 == 0 {
                    ctx.check()?;
                }
                if matches!(bytes[position], b'\\' | b'\r' | b'\n')
                    || bytes[position].is_ascii_control()
                    || (byte == b'`' && bytes[position..].starts_with(b"${"))
                {
                    return Err(blocked());
                }
                position += 1;
            }
            if position == bytes.len() {
                return Err(blocked());
            }
            position += 1;
        } else if !matches!(
            byte,
            b'=' | b';'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'.'
                | b'*'
                | b'+'
                | b','
                | b'!'
                | b'>'
                | b'-'
                | b':'
        ) {
            return Err(blocked());
        }
        match byte {
            b'(' | b'{' => {
                depth += 1;
                if depth > limits.max_html_depth {
                    return Err(blocked());
                }
            }
            b')' | b'}' => depth = depth.checked_sub(1).ok_or_else(blocked)?,
            _ => {}
        }
        tokens.push(&body[start..position]);
    }
    ctx.check()?;
    Ok(Parser {
        tokens,
        position: 0,
    })
}

pub(super) fn solve(
    body: &str,
    base: &Url,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<Url, Error> {
    let mut parser = tokenize(body, ctx, limits)?;
    parser.scaffolding()?;
    let assigned = parser.take("window");
    if assigned {
        parser.sequence(&[".", "execDeep", "=", "function", "(", ")", "{"])?;
        parser.scaffolding()?;
    }
    let parenthesized = !assigned && parser.take("(");
    let negated = !assigned && !parenthesized && parser.take("!");
    if parenthesized || negated {
        if parser.take("function") {
            parser.sequence(&["(", ")"])?;
        } else if parenthesized {
            parser.sequence(&["(", ")", "=", ">"])?;
        } else {
            return Err(blocked());
        }
        parser.expect("{")?;
        parser.scaffolding()?;
    }
    parser.sequence(&["let", "jsa", "="])?;
    let mut value = parser.number()?;
    parser.expect(";")?;
    let guarded = parser.take("try");
    if guarded {
        parser.expect("{")?;
    }
    let mut functions = HashMap::new();
    let mut operations = 0;
    loop {
        ctx.check()?;
        if parser.take("let") {
            let name = parser.identifier()?;
            if functions.len() >= MAX_FUNCTIONS || functions.contains_key(name) {
                return Err(blocked());
            }
            functions.insert(name, parser.operation()?);
        } else if parser.take("jsa") {
            operations += 1;
            if operations > MAX_OPERATIONS {
                return Err(blocked());
            }
            parser.expect("=")?;
            let name = parser.identifier()?;
            parser.sequence(&["(", "jsa", ")", ";"])?;
            let operation = functions.get(name).ok_or_else(blocked)?;
            value = match operation {
                Operation::Multiply(factor) => value.checked_mul(*factor),
                Operation::Add(amount) => value.checked_add(*amount),
            }
            .filter(|n| *n <= MAX_INTEGER)
            .ok_or_else(blocked)?;
        } else {
            break;
        }
    }
    if operations == 0 {
        return Err(blocked());
    }
    if guarded {
        parser.sequence(&[
            "}", "catch", "(", "e", ")", "{", "jsa", "=", "-", "1", ";", "}",
        ])?;
    }
    parser.sequence(&["DDG", ".", "deep", ".", "initialize", "("])?;
    let prefix = parser.quoted()?;
    parser.sequence(&["+", "jsa"])?;
    if parser.take(",") {
        parser.expect("false")?;
    }
    parser.sequence(&[")", ";"])?;
    if assigned {
        parser.sequence(&["return", "{", "isJsaChallenge", ":", "true", "}", ";"])?;
    }
    parser.scaffolding()?;
    if assigned {
        parser.sequence(&["}", ";"])?;
        parser.scaffolding()?;
    } else if parenthesized || negated {
        parser.expect("}")?;
        if parenthesized && parser.take(")") {
            parser.sequence(&["(", ")", ";"])?;
        } else {
            parser.sequence(&["(", ")"])?;
            if parenthesized {
                parser.expect(")")?;
            }
            parser.expect(";")?;
        }
        parser.scaffolding()?;
    }
    if parser.position != parser.tokens.len() {
        return Err(blocked());
    }
    if !super::is_web_url(base, true) {
        return Err(blocked());
    }
    let target = format!("{prefix}{value}");
    let origin = Url::parse("https://links.duckduckgo.com/").map_err(|_| blocked())?;
    let url = provider_target(&target, &origin, limits)?;
    ctx.check()?;
    Ok(url)
}
