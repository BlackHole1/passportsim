//! A small Rust tokenizer for the source rules of `xtask layering`: comments are
//! dropped, string literals keep their text (rule 5 looks inside them), numbers, char literals
//! and lifetimes produce no token.

/// Token kinds the source rules need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    /// Identifier or keyword (`r#name` yields `name`).
    Ident(String),
    /// `::`.
    PathSep,
    /// Any other punctuation character.
    Punct(char),
    /// Contents of a string, byte string, C string or raw string literal (escapes kept as written).
    Str(String),
}

/// A token and its 1-based line.
#[derive(Clone, Debug)]
pub struct Token {
    /// Kind and text.
    pub tok: Tok,
    /// 1-based line of the first character.
    pub line: usize,
}

impl Token {
    /// Whether this token is the identifier `name`.
    pub fn is_ident(&self, name: &str) -> bool {
        matches!(&self.tok, Tok::Ident(s) if s == name)
    }

    /// Whether this token is the punctuation `c`.
    pub fn is_punct(&self, c: char) -> bool {
        self.tok == Tok::Punct(c)
    }
}

/// Tokenizes Rust source text.
pub fn lex(src: &str) -> Vec<Token> {
    let s: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < s.len() {
        let c = s[i];
        let start_line = line;
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && s.get(i + 1) == Some(&'/') {
            while i < s.len() && s[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && s.get(i + 1) == Some(&'*') {
            i = skip_block_comment(&s, i, &mut line);
        } else if let Some((body_start, hashes)) = raw_string_start(&s, i) {
            let (text, end) = raw_string(&s, body_start, hashes, &mut line);
            out.push(tok(Tok::Str(text), start_line));
            i = end;
        } else if c == '"' || (matches!(c, 'b' | 'c') && s.get(i + 1) == Some(&'"')) {
            let open = if c == '"' { i } else { i + 1 };
            let (text, end) = quoted_string(&s, open + 1, &mut line);
            out.push(tok(Tok::Str(text), start_line));
            i = end;
        } else if c == 'r'
            && s.get(i + 1) == Some(&'#')
            && s.get(i + 2).is_some_and(|&n| is_ident_start(n))
        {
            let end = ident_end(&s, i + 2);
            out.push(tok(Tok::Ident(s[i + 2..end].iter().collect()), start_line));
            i = end;
        } else if is_ident_start(c) {
            let end = ident_end(&s, i);
            out.push(tok(Tok::Ident(s[i..end].iter().collect()), start_line));
            i = end;
        } else if c.is_ascii_digit() {
            i = number_end(&s, i);
        } else if c == '\'' {
            i = skip_quote(&s, i, &mut line);
        } else if c == ':' && s.get(i + 1) == Some(&':') {
            out.push(tok(Tok::PathSep, start_line));
            i += 2;
        } else {
            out.push(tok(Tok::Punct(c), start_line));
            i += 1;
        }
    }
    out
}

fn tok(tok: Tok, line: usize) -> Token {
    Token { tok, line }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn ident_end(s: &[char], mut i: usize) -> usize {
    while i < s.len() && (s[i] == '_' || s[i].is_alphanumeric()) {
        i += 1;
    }
    i
}

fn number_end(s: &[char], mut i: usize) -> usize {
    loop {
        while i < s.len() && (s[i] == '_' || s[i].is_alphanumeric()) {
            i += 1;
        }
        // A fraction, but not a range `0..n` or a method call `1.max(2)`.
        if s.get(i) == Some(&'.') && s.get(i + 1).is_some_and(char::is_ascii_digit) {
            i += 1;
        } else {
            return i;
        }
    }
}

fn skip_block_comment(s: &[char], mut i: usize, line: &mut usize) -> usize {
    let mut depth = 0;
    while i < s.len() {
        if s[i] == '/' && s.get(i + 1) == Some(&'*') {
            depth += 1;
            i += 2;
        } else if s[i] == '*' && s.get(i + 1) == Some(&'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            if s[i] == '\n' {
                *line += 1;
            }
            i += 1;
        }
    }
    i
}

/// `r"`, `r#"`, `br"`, `br#"`, `cr"` and `cr#"`: the index after the opening quote and the
/// number of `#`.
fn raw_string_start(s: &[char], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if matches!(s[j], 'b' | 'c') {
        j += 1;
    }
    if s.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = s[j..].iter().take_while(|&&c| c == '#').count();
    j += hashes;
    (s.get(j) == Some(&'"')).then_some((j + 1, hashes))
}

fn raw_string(s: &[char], mut i: usize, hashes: usize, line: &mut usize) -> (String, usize) {
    let start = i;
    while i < s.len() {
        if s[i] == '"'
            && s[i + 1..]
                .iter()
                .take(hashes)
                .filter(|&&c| c == '#')
                .count()
                == hashes
        {
            return (s[start..i].iter().collect(), i + 1 + hashes);
        }
        if s[i] == '\n' {
            *line += 1;
        }
        i += 1;
    }
    (s[start..].iter().collect(), i)
}

fn quoted_string(s: &[char], mut i: usize, line: &mut usize) -> (String, usize) {
    let start = i;
    while i < s.len() && s[i] != '"' {
        if s[i] == '\\' {
            i += 1;
        }
        if s.get(i) == Some(&'\n') {
            *line += 1;
        }
        i += 1;
    }
    let end = i.min(s.len());
    (s[start..end].iter().collect(), (i + 1).min(s.len()))
}

/// A char literal (`'a'`, `'\n'`, `'\u{1F600}'`) or a lifetime or label (`'a`).
fn skip_quote(s: &[char], i: usize, line: &mut usize) -> usize {
    if s.get(i + 1) == Some(&'\\') {
        let mut j = i + 3;
        while j < s.len() && s[j] != '\'' && s[j] != '\n' {
            j += 1;
        }
        return (j + 1).min(s.len());
    }
    if s.get(i + 2) == Some(&'\'') {
        if s.get(i + 1) == Some(&'\n') {
            *line += 1;
        }
        return i + 3;
    }
    if s.get(i + 1).is_some_and(|&c| is_ident_start(c)) {
        return ident_end(s, i + 1);
    }
    i + 1
}
