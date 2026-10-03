//! Parser for EngineData, the PostScript-like text engine dump embedded in type layers.

use crate::error::{bail, Result};

/// A node of EngineData.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    Dict(Vec<(String, Node)>),
    Array(Vec<Node>),
    Number(f64),
    Bool(bool),
    String(String),
    Name(String),
}

static EMPTY: Node = Node::Dict(Vec::new());

impl Node {
    pub fn empty() -> &'static Node {
        &EMPTY
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entries().iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn path(&self, keys: &[&str]) -> Option<&Node> {
        keys.iter().try_fold(self, |node, k| node.get(k))
    }

    pub fn num(&self) -> Option<f64> {
        match self {
            Node::Number(n) => Some(*n),
            Node::Bool(b) => Some(*b as u8 as f64),
            _ => None,
        }
    }

    pub fn boolean(&self) -> Option<bool> {
        match self {
            Node::Bool(b) => Some(*b),
            Node::Number(n) => Some(*n != 0.0),
            _ => None,
        }
    }

    pub fn array(&self) -> &[Node] {
        match self {
            Node::Array(a) => a,
            _ => &[],
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Node::String(s) | Node::Name(s) => Some(s),
            _ => None,
        }
    }

    pub fn entries(&self) -> &[(String, Node)] {
        match self {
            Node::Dict(v) => v,
            _ => &[],
        }
    }
}

const DELIMITERS: &[u8] = b"[]<>/(";

struct Parser<'a> {
    b: &'a [u8],
    p: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.p).copied()
    }

    fn rest(&self) -> &[u8] {
        &self.b[self.p.min(self.b.len())..]
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.p += 1;
        }
    }

    fn token(&mut self) -> &[u8] {
        let start = self.p;
        while self.peek().is_some_and(|c| !c.is_ascii_whitespace() && !DELIMITERS.contains(&c)) {
            self.p += 1;
        }
        &self.b[start..self.p]
    }

    fn name(&mut self) -> String {
        self.p += 1;
        String::from_utf8_lossy(self.token()).into_owned()
    }

    fn value(&mut self, depth: usize) -> Result<Node> {
        if depth > 256 {
            bail!("EngineData nested too deeply");
        }
        self.skip_ws();
        let Some(c) = self.peek() else { bail!("EngineData ended unexpectedly") };
        if self.rest().starts_with(b"<<") {
            self.p += 2;
            let mut entries = vec![];
            loop {
                self.skip_ws();
                if self.rest().starts_with(b">>") {
                    self.p += 2;
                    break;
                }
                match self.peek() {
                    None => break,
                    Some(b'/') => {
                        let k = self.name();
                        entries.push((k, self.value(depth + 1)?));
                    }
                    Some(_) => self.p += 1,
                }
            }
            return Ok(Node::Dict(entries));
        }
        match c {
            b'[' => {
                self.p += 1;
                let mut items = vec![];
                loop {
                    self.skip_ws();
                    match self.peek() {
                        None => break,
                        Some(b']') => {
                            self.p += 1;
                            break;
                        }
                        Some(_) => items.push(self.value(depth + 1)?),
                    }
                }
                Ok(Node::Array(items))
            }
            b'/' => Ok(Node::Name(self.name())),
            b'(' => Ok(Node::String(self.string())),
            _ => {
                let t = self.token();
                if t.is_empty() {
                    self.p += 1;
                    return Ok(Node::Name(String::new()));
                }
                Ok(match t {
                    b"true" => Node::Bool(true),
                    b"false" => Node::Bool(false),
                    _ => Node::Number(std::str::from_utf8(t).ok().and_then(|s| s.parse().ok()).unwrap_or(0.0)),
                })
            }
        }
    }

    fn string(&mut self) -> String {
        self.p += 1;
        let mut raw = vec![];
        while let Some(c) = self.peek() {
            match c {
                b'\\' if self.p + 1 < self.b.len() => {
                    raw.push(self.b[self.p + 1]);
                    self.p += 2;
                }
                b')' => {
                    self.p += 1;
                    break;
                }
                _ => {
                    raw.push(c);
                    self.p += 1;
                }
            }
        }
        match raw.strip_prefix(&[0xFE, 0xFF]) {
            Some(utf16) => {
                let units: Vec<u16> = utf16.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                String::from_utf16_lossy(&units)
            }
            None => String::from_utf8_lossy(&raw).into_owned(),
        }
    }
}

pub(crate) fn parse(data: &[u8]) -> Result<Node> {
    Parser { b: data, p: 0 }.value(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dicts_arrays_and_scalars() {
        let n = parse(b"<< /A 1.5 /B [ 1 2 .5 ] /C true /D /Name /E << /F -3 >> >>").unwrap();
        assert_eq!(n.get("A").and_then(Node::num), Some(1.5));
        assert_eq!(n.get("B").map(|b| b.array().len()), Some(3));
        assert_eq!(n.get("B").unwrap().array()[2].num(), Some(0.5));
        assert_eq!(n.get("C").and_then(Node::boolean), Some(true));
        assert_eq!(n.get("D").and_then(Node::str), Some("Name"));
        assert_eq!(n.path(&["E", "F"]).and_then(Node::num), Some(-3.0));
        assert!(n.path(&["E", "G"]).is_none());
    }

    #[test]
    fn decodes_utf16_strings_with_escapes() {
        let mut src = b"<< /T (".to_vec();
        src.extend_from_slice(&[0xFE, 0xFF, 0x00, b'a', 0x00, b'\\', b'(', 0x00, 0xF1]);
        src.extend_from_slice(b") >>");
        assert_eq!(parse(&src).unwrap().get("T").and_then(Node::str), Some("a(ñ"));
    }

    #[test]
    fn tolerates_truncation() {
        let n = parse(b"<< /A [ 1 2").unwrap();
        assert_eq!(n.get("A").map(|a| a.array().len()), Some(2));
        assert!(parse(b"   ").is_err());
    }

    #[test]
    fn rejects_pathological_nesting() {
        assert!(parse(&b"[".repeat(1000)).is_err());
    }
}
