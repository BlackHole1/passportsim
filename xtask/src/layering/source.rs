//! Per-file source scan: which code sits behind a test or device gate, which
//! out-of-line modules a file declares, and the raw findings of layering rules 1 and 5.
//!
//! How gated code is recognized (on tokens, comments excluded):
//! - an outer attribute `#[cfg(P)]` gates the item that follows it; `#[test]` (or a path ending
//!   in `::test`) gates the function that follows it;
//! - an inner attribute `#![cfg(P)]` gates the enclosing `{ ... }` block, or the whole file at
//!   the top level;
//! - `P` counts as a test gate when it implies `test`: `test`, or `all(..)` with an element that
//!   implies it, or `any(..)` whose elements all imply it; `not(..)` never implies it. The same
//!   reading with `feature = "device"` gives the device gate;
//! - an item runs from its attributes to the first `;` or the end of its first `{ ... }` block
//!   at its own bracket depth (`let`, `use`, `type`, `static` and non-fn `const` run to `;`);
//! - `mod name;` behind a gate passes the gate to the module's file (see `walk.rs`).
//!
//! Known limits: macro-generated code is not seen, and an attributed `let x = if c { .. } else
//! { .. };` is not special-cased beyond running to `;`.

use super::lexer::{Tok, Token, lex};
use super::policy::{
    self, DEVICE_FEATURE, FORBIDDEN_COLLECTION_MODULES, FORBIDDEN_IDENTS, FORBIDDEN_STD_MODULES,
};

/// The gates code sits behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gate {
    /// Compiled only for tests, or part of a test, bench, example or build-script target.
    pub test: bool,
    /// Compiled only with `feature = "device"`.
    pub device: bool,
}

impl Gate {
    /// Gated by either.
    pub fn or(self, other: Gate) -> Gate {
        Gate {
            test: self.test || other.test,
            device: self.device || other.device,
        }
    }

    /// Gated by both.
    pub fn and(self, other: Gate) -> Gate {
        Gate {
            test: self.test && other.test,
            device: self.device && other.device,
        }
    }
}

/// An out-of-line `mod name;` declaration.
#[derive(Clone, Debug)]
pub struct ModDecl {
    /// Module name.
    pub name: String,
    /// Value of a `#[path = ".."]` attribute.
    pub path_attr: Option<String>,
    /// Names of the inline `mod x { .. }` blocks around the declaration, outermost first.
    pub inline_parents: Vec<String>,
    /// Gate of the declaration, including the file's own gate.
    pub gate: Gate,
}

/// A raw rule finding in one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// 1-based line.
    pub line: usize,
    /// What was found, for example `std::time` or `HashMap`.
    pub what: String,
    /// Gate of the code the finding sits in.
    pub gate: Gate,
}

/// Result of scanning one file.
#[derive(Debug, Default)]
pub struct FileScan {
    /// Rule 1 findings (forbidden std APIs), gated or not.
    pub std_uses: Vec<Finding>,
    /// Rule 5 findings (device paths in string literals), gated or not.
    pub device_refs: Vec<Finding>,
    /// Rule 5 findings at a call site that opens or configures a terminal device.
    /// They are separate because a short audited list of files may name them.
    pub port_opens: Vec<Finding>,
    /// Out-of-line module declarations.
    pub mods: Vec<ModDecl>,
}

/// Scans one file whose own gate (from its module declaration or target kind) is `file_gate`.
pub fn scan(text: &str, file_gate: Gate) -> FileScan {
    let toks = lex(text);
    let close = matching(&toks);
    let mut gates = vec![file_gate; toks.len()];
    let mut mods = Vec::new();
    // Open braces around the cursor, with the inline module name a brace belongs to.
    let mut braces: Vec<(usize, Option<String>)> = Vec::new();
    let mut path_attr: Option<String> = None;
    let mut i = 0;
    while i < toks.len() {
        if let Some((end, inner)) = attribute_at(&toks, &close, i) {
            let body = &toks[if inner { i + 3 } else { i + 2 }..end];
            let gate = attr_gate(body);
            if gate != Gate::default() {
                let (from, to) = if inner {
                    match braces.last() {
                        Some(&(open, _)) => (open, close[open].unwrap_or(toks.len() - 1)),
                        None => (0, toks.len() - 1),
                    }
                } else {
                    (i, item_end(&toks, &close, end + 1))
                };
                for g in &mut gates[from..=to.min(toks.len() - 1)] {
                    *g = g.or(gate);
                }
            }
            if let [key, eq, value] = body
                && key.is_ident("path")
                && eq.is_punct('=')
                && let Tok::Str(p) = &value.tok
            {
                path_attr = Some(p.clone());
            }
            i = end + 1;
            continue;
        }
        let t = &toks[i];
        if t.is_ident("mod")
            && let Some(Tok::Ident(name)) = toks.get(i + 1).map(|t| &t.tok)
        {
            match toks.get(i + 2) {
                Some(next) if next.is_punct(';') => mods.push(ModDecl {
                    name: name.clone(),
                    path_attr: path_attr.take(),
                    inline_parents: braces.iter().filter_map(|b| b.1.clone()).collect(),
                    gate: gates[i],
                }),
                Some(next) if next.is_punct('{') => {
                    braces.push((i + 2, Some(name.clone())));
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        if t.is_punct('{') {
            braces.push((i, None));
        } else if t.is_punct('}') {
            braces.pop();
        }
        // A `#[path]` attribute belongs to the next item only.
        if t.is_punct('{') || t.is_punct('}') || t.is_punct(';') {
            path_attr = None;
        }
        i += 1;
    }
    let (std_uses, device_refs, port_opens) = findings(&toks, &gates);
    FileScan {
        std_uses,
        device_refs,
        port_opens,
        mods,
    }
}

const TEST: Gate = Gate {
    test: true,
    device: false,
};
const DEVICE: Gate = Gate {
    test: false,
    device: true,
};

/// For every opening bracket, the index of its closing bracket.
fn matching(toks: &[Token]) -> Vec<Option<usize>> {
    let mut out = vec![None; toks.len()];
    let mut stack = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::Punct('(' | '[' | '{') => stack.push(i),
            Tok::Punct(')' | ']' | '}') => {
                if let Some(open) = stack.pop() {
                    out[open] = Some(i);
                }
            }
            _ => {}
        }
    }
    out
}

/// `#[..]` or `#![..]` starting at `i`: the index of its `]` and whether it is inner.
fn attribute_at(toks: &[Token], close: &[Option<usize>], i: usize) -> Option<(usize, bool)> {
    if !toks.get(i)?.is_punct('#') {
        return None;
    }
    let inner = toks.get(i + 1).is_some_and(|t| t.is_punct('!'));
    let open = if inner { i + 2 } else { i + 1 };
    if !toks.get(open)?.is_punct('[') {
        return None;
    }
    close[open].map(|end| (end, inner))
}

/// The gate an attribute body (the tokens between `[` and `]`) puts on its item.
fn attr_gate(body: &[Token]) -> Gate {
    match body {
        [t] if t.is_ident("test") => TEST,
        [.., sep, t] if sep.tok == Tok::PathSep && t.is_ident("test") => TEST,
        [name, open, pred @ .., end]
            if name.is_ident("cfg") && open.is_punct('(') && end.is_punct(')') =>
        {
            implied(pred)
        }
        _ => Gate::default(),
    }
}

/// The gates a `cfg` predicate implies.
fn implied(pred: &[Token]) -> Gate {
    match pred {
        [t] if t.is_ident("test") => TEST,
        [f, eq, v] if f.is_ident("feature") && eq.is_punct('=') => match &v.tok {
            Tok::Str(s) if s == DEVICE_FEATURE => DEVICE,
            _ => Gate::default(),
        },
        [op, open, inner @ .., end] if open.is_punct('(') && end.is_punct(')') => {
            let elems: Vec<&[Token]> = split_commas(inner);
            if op.is_ident("all") {
                elems.iter().fold(Gate::default(), |g, e| g.or(implied(e)))
            } else if op.is_ident("any") {
                elems
                    .iter()
                    .map(|e| implied(e))
                    .reduce(Gate::and)
                    .unwrap_or_default()
            } else {
                Gate::default()
            }
        }
        _ => Gate::default(),
    }
}

/// Non-empty comma-separated parts at bracket depth 0.
fn split_commas(toks: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::Punct('(' | '[' | '{') => depth += 1,
            Tok::Punct(')' | ']' | '}') => depth -= 1,
            Tok::Punct(',') if depth == 0 => {
                parts.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&toks[start..]);
    parts.retain(|p| !p.is_empty());
    parts
}

/// Last token of the item that starts at `j` (after an outer attribute).
fn item_end(toks: &[Token], close: &[Option<usize>], mut j: usize) -> usize {
    let last = toks.len().saturating_sub(1);
    while let Some((end, false)) = attribute_at(toks, close, j) {
        j = end + 1;
    }
    let mut k = j;
    if toks.get(k).is_some_and(|t| t.is_ident("pub")) {
        k += 1;
        if toks.get(k).is_some_and(|t| t.is_punct('(')) {
            k = close[k].map_or(last, |c| c + 1);
        }
    }
    let to_semicolon = match toks.get(k) {
        Some(t)
            if ["let", "use", "type", "static"]
                .iter()
                .any(|kw| t.is_ident(kw)) =>
        {
            true
        }
        Some(t) if t.is_ident("const") => !toks.get(k + 1).is_some_and(|n| {
            ["fn", "unsafe", "async", "extern"]
                .iter()
                .any(|kw| n.is_ident(kw))
        }),
        _ => false,
    };
    while j < toks.len() {
        match toks[j].tok {
            Tok::Punct(open @ ('(' | '[' | '{')) => {
                let Some(c) = close[j] else { return last };
                if open == '{' && !to_semicolon {
                    return c;
                }
                j = c + 1;
            }
            Tok::Punct(';') => return j,
            Tok::Punct(')' | ']' | '}') => return j.saturating_sub(1),
            _ => j += 1,
        }
    }
    last
}

/// Rule 1 and rule 5 findings, each with the gate of its token.
fn findings(toks: &[Token], gates: &[Gate]) -> (Vec<Finding>, Vec<Finding>, Vec<Finding>) {
    let mut std_uses = Vec::new();
    let mut device_refs = Vec::new();
    let mut port_opens = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let gate = gates[i];
        match &t.tok {
            Tok::Ident(name) if name == "std" => match toks.get(i + 1).map(|n| &n.tok) {
                Some(Tok::PathSep) => {
                    for (line, what) in std_paths(toks, i + 2) {
                        std_uses.push(Finding { line, what, gate });
                    }
                }
                Some(Tok::Ident(kw)) if kw == "as" => std_uses.push(Finding {
                    line: t.line,
                    what: "std as <alias>".to_string(),
                    gate,
                }),
                _ => {}
            },
            Tok::Ident(name) if FORBIDDEN_IDENTS.contains(&name.as_str()) => {
                std_uses.push(Finding {
                    line: t.line,
                    what: name.clone(),
                    gate,
                });
            }
            Tok::Ident(name) => {
                if let Some(what) = policy::port_open_ident(name) {
                    port_opens.push(Finding {
                        line: t.line,
                        what,
                        gate,
                    });
                }
            }
            Tok::Str(s) => {
                if let Some(what) = policy::device_literal(s) {
                    device_refs.push(Finding {
                        line: t.line,
                        what,
                        gate,
                    });
                }
            }
            _ => {}
        }
    }
    (std_uses, device_refs, port_opens)
}

/// Forbidden paths below `std::`, where `j` is the token after `std::` (use groups expanded).
fn std_paths(toks: &[Token], j: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let Some(t) = toks.get(j) else { return out };
    match &t.tok {
        Tok::Ident(m) if FORBIDDEN_STD_MODULES.contains(&m.as_str()) => {
            out.push((t.line, format!("std::{m}")));
        }
        Tok::Ident(m)
            if m == "collections" && toks.get(j + 1).is_some_and(|n| n.tok == Tok::PathSep) =>
        {
            let segs = match toks.get(j + 2) {
                Some(n) if n.is_punct('{') => group_elements(toks, j + 2),
                _ => vec![j + 2],
            };
            for k in segs {
                if let Some(Token {
                    tok: Tok::Ident(seg),
                    line,
                }) = toks.get(k)
                    && FORBIDDEN_COLLECTION_MODULES.contains(&seg.as_str())
                {
                    out.push((*line, format!("std::collections::{seg}")));
                }
            }
        }
        Tok::Punct('*') => out.push((t.line, "std::*".to_string())),
        Tok::Punct('{') => {
            for k in group_elements(toks, j) {
                out.extend(std_paths(toks, k));
            }
        }
        _ => {}
    }
    out
}

/// Start indices of the comma-separated elements of the group opened at `open`.
fn group_elements(toks: &[Token], open: usize) -> Vec<usize> {
    let mut starts = vec![open + 1];
    let mut depth = 0;
    for (k, t) in toks.iter().enumerate().skip(open) {
        match t.tok {
            Tok::Punct('(' | '[' | '{') => depth += 1,
            Tok::Punct(')' | ']' | '}') => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Tok::Punct(',') if depth == 1 => starts.push(k + 1),
            _ => {}
        }
    }
    starts
}
