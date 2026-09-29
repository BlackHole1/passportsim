//! `cargo xtask comment-check [--base <rev>]`: proves that a change edits comments only.
//!
//! For every `.rs` file that differs between the merge base of `<rev>` (default `main`) and
//! `HEAD` and the working tree, both versions are lexed with `proc-macro2` and compared as token
//! streams with comments and `#[doc = "..."]` attributes removed. A file whose streams differ is a
//! code change and is named in the failure. Added, deleted and unlexable files fail too.
//!
//! Some comment text is read by machines, so it counts as code here:
//! - the first non-empty doc line of each `#[command]` handler, which `pemu-macros` reuses as CLI
//!   help, the MCP tool description and `docs/commands/*.md`;
//! - a doc attribute whose value is not one string literal (`concat!`, `include_str!`);
//! - the marker comments in [`FROZEN_MARKERS`], which tests slice source text by.
//!
//! Not covered: `Cargo.toml` and other non-Rust files, doctest bodies (run by `cargo test`), and
//! the source-text tests over `xtask/src/ci/tiers.rs` (run `cargo test -p xtask` after editing it).

use std::path::Path;
use std::str::FromStr;

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};

const USAGE: &str = "usage: cargo xtask comment-check [--base <rev>]\n\
\n\
Fails unless every .rs file changed since the merge base of <rev> (default main) and HEAD,\n\
working tree included, differs only in comments and doc text. A command's first doc line counts\n\
as code, because CLI help, MCP descriptions and docs/commands are generated from it.";

/// Comment lines that tests find by text, so removing one is a behaviour change.
const FROZEN_MARKERS: &[(&str, &str)] = &[(
    // The Windows `enumeration_opens_nothing` test slices its own source up to this line.
    "crates/pemu-host/src/platform/serial.rs",
    "// End of the SetupAPI arm.",
)];

pub fn run(args: &[String]) -> Result<(), String> {
    let mut base = "main".to_string();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--base" => {
                base = rest
                    .next()
                    .ok_or_else(|| format!("--base needs a revision\n{USAGE}"))?
                    .clone();
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let root = crate::codegen::workspace_root();
    let merge_base = git_output(&root, &["merge-base", &base, "HEAD"])?;
    let merge_base = merge_base.trim();
    let listing = git_output(
        &root,
        &[
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            merge_base,
            "--",
            "*.rs",
        ],
    )?;
    let mut fields = listing.split('\0').filter(|f| !f.is_empty());
    let mut checked = 0usize;
    let mut failures = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        checked += 1;
        let verdict = match status {
            "M" | "T" => {
                let old = git_output(&root, &["show", &format!("{merge_base}:{path}")])?;
                let new = std::fs::read_to_string(root.join(path))
                    .map_err(|err| format!("cannot read {path}: {err}"))?;
                compare(path, &old, &new)
            }
            "A" => Err("added file".to_string()),
            "D" => Err("deleted file".to_string()),
            other => Err(format!("unexpected diff status `{other}`")),
        };
        if let Err(why) = verdict {
            failures.push(format!("{path}: {why}"));
        }
    }
    let untracked = git_output(
        &root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            "*.rs",
        ],
    )?;
    for path in untracked.split('\0').filter(|f| !f.is_empty()) {
        checked += 1;
        failures.push(format!("{path}: added file (untracked)"));
    }
    println!(
        "comment-check: {checked} changed .rs file(s) since {} ({base}), {} with code changes",
        &merge_base[..merge_base.len().min(12)],
        failures.len()
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "not a comment-only change:\n  {}",
            failures.join("\n  ")
        ))
    }
}

/// `Ok` when `new` differs from `old` in comments and free doc text only; otherwise why not.
pub fn compare(path: &str, old: &str, new: &str) -> Result<(), String> {
    let old_tokens = code_tokens(old).map_err(|err| format!("base does not lex: {err}"))?;
    let new_tokens = code_tokens(new).map_err(|err| format!("does not lex: {err}"))?;
    if let Some(at) = old_tokens.iter().zip(&new_tokens).position(|(a, b)| a != b) {
        return Err(format!(
            "code differs at token {at}: `{}` became `{}`",
            old_tokens[at], new_tokens[at]
        ));
    }
    if old_tokens.len() != new_tokens.len() {
        return Err(format!(
            "code differs: {} tokens became {}",
            old_tokens.len(),
            new_tokens.len()
        ));
    }
    for (file, marker) in FROZEN_MARKERS {
        if path == *file && has_line(old, marker) && !has_line(new, marker) {
            return Err(format!("marker comment `{marker}` removed"));
        }
    }
    Ok(())
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l.trim() == line)
}

/// The file's tokens with comments and free doc text removed, one string per token. Delimiters
/// and joint punctuation are kept so `a::b` and `a: :b` differ.
pub fn code_tokens(text: &str) -> Result<Vec<String>, String> {
    let stream = TokenStream::from_str(text).map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    walk(stream, &mut out);
    Ok(out)
}

fn walk(stream: TokenStream, out: &mut Vec<String>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        // A run of attributes `#[..]` / `#![..]` is taken as one unit, so a command's summary is
        // found whether its doc lines come before or after `#[command(..)]`.
        // Inner attributes (`//!`) form their own run: they belong to the enclosing module, not
        // to the first item after them.
        let mut attrs = Vec::new();
        let mut j = i;
        let mut run_inner = None;
        while let Some((group, inner, next)) = attribute_at(&tokens, j) {
            if *run_inner.get_or_insert(inner) != inner {
                break;
            }
            attrs.push((j, group, next));
            j = next;
        }
        if attrs.is_empty() {
            push_token(&tokens[i], out);
            i += 1;
            continue;
        }
        // The summary token leads the run, so moving doc lines across `#[command]` is no change.
        if attrs.iter().any(|(_, group, _)| is_command_attr(group)) {
            let summary = attrs
                .iter()
                .filter_map(|(_, group, _)| free_doc_text(group))
                .map(|text| text.trim().to_string())
                .find(|line| !line.is_empty());
            out.push(format!("command-summary:{}", summary.unwrap_or_default()));
        }
        for (start, group, next) in attrs {
            if free_doc_text(group).is_none() {
                for token in &tokens[start..next] {
                    push_token(token, out);
                }
            }
        }
        i = j;
    }
}

fn push_token(token: &TokenTree, out: &mut Vec<String>) {
    match token {
        TokenTree::Group(group) => {
            let (open, close) = match group.delimiter() {
                Delimiter::Parenthesis => ("(", ")"),
                Delimiter::Brace => ("{", "}"),
                Delimiter::Bracket => ("[", "]"),
                Delimiter::None => ("", ""),
            };
            out.push(open.to_string());
            walk(group.stream(), out);
            out.push(close.to_string());
        }
        TokenTree::Punct(punct) => {
            let joint = if punct.spacing() == Spacing::Joint {
                "~"
            } else {
                ""
            };
            out.push(format!("{}{joint}", punct.as_char()));
        }
        other => out.push(other.to_string()),
    }
}

/// The bracket group of an attribute starting at `at` (`#` then optional `!` then `[..]`), whether
/// it is an inner attribute, and the index after it.
fn attribute_at(tokens: &[TokenTree], at: usize) -> Option<(&proc_macro2::Group, bool, usize)> {
    let TokenTree::Punct(hash) = tokens.get(at)? else {
        return None;
    };
    if hash.as_char() != '#' {
        return None;
    }
    let mut next = at + 1;
    let inner = matches!(tokens.get(next), Some(TokenTree::Punct(bang)) if bang.as_char() == '!');
    if inner {
        next += 1;
    }
    match tokens.get(next)? {
        TokenTree::Group(group) if group.delimiter() == Delimiter::Bracket => {
            Some((group, inner, next + 1))
        }
        _ => None,
    }
}

/// The value of `doc = "<literal>"`, decoded; `None` for any other attribute, including a doc
/// attribute built by a macro call.
fn free_doc_text(group: &proc_macro2::Group) -> Option<String> {
    let parts: Vec<TokenTree> = group.stream().into_iter().collect();
    match parts.as_slice() {
        [
            TokenTree::Ident(name),
            TokenTree::Punct(eq),
            TokenTree::Literal(lit),
        ] if name == "doc" && eq.as_char() == '=' => decode_str_literal(&lit.to_string()),
        _ => None,
    }
}

/// `command(..)` or a path ending in `command` followed by its arguments.
fn is_command_attr(group: &proc_macro2::Group) -> bool {
    let mut last_ident = None;
    for token in group.stream() {
        match token {
            TokenTree::Ident(ident) => last_ident = Some(ident.to_string()),
            TokenTree::Punct(p) if p.as_char() == ':' => {}
            TokenTree::Group(_) => break,
            _ => return false,
        }
    }
    last_ident.as_deref() == Some("command")
}

/// Decodes a string literal as `proc-macro2` prints it: `"..."` with escapes, or raw `r#"..."#`.
fn decode_str_literal(repr: &str) -> Option<String> {
    if let Some(raw) = repr.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let body = raw.get(hashes + 1..raw.len().checked_sub(hashes + 1)?)?;
        return Some(body.to_string());
    }
    let body = repr.strip_prefix('"')?.strip_suffix('"')?;
    let mut text = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        match chars.next()? {
            'n' => text.push('\n'),
            't' => text.push('\t'),
            'r' => text.push('\r'),
            '0' => text.push('\0'),
            '\\' => text.push('\\'),
            '"' => text.push('"'),
            '\'' => text.push('\''),
            'u' => {
                let hex: String = chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                text.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            'x' => {
                let hex: String = chars.by_ref().take(2).collect();
                text.push(char::from(u8::from_str_radix(&hex, 16).ok()?));
            }
            '\n' => {
                // A line continuation skips the newline and the next line's leading whitespace.
                let rest: String = chars.collect();
                text.push_str(&decode_str_literal(&format!("\"{}\"", rest.trim_start()))?);
                return Some(text);
            }
            _ => return None,
        }
    }
    Some(text)
}

/// Stdout of `git -C root <args>` as text.
fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(crate::util::git(root, args)?)
        .map_err(|_| format!("git {} printed non-UTF-8", args[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"//! Module header.

/// Advance virtual time, or wait until a matcher fires.
///
/// Longer text that a comment edit may cut.
#[command(name = "run", group = "time")]
pub fn run(args: Value) -> Result<Output, ApiError> {
    // Why the budget is doubled.
    let budget = args.len() * 2; // trailing note
    /* block comment */
    Ok(Output::new(budget))
}

/// A helper.
#[inline]
fn helper() -> &'static str {
    "a string literal is code"
}
"#;

    fn edited(from: &str, to: &str) -> String {
        assert!(BASE.contains(from), "fixture lacks {from:?}");
        BASE.replacen(from, to, 1)
    }

    #[test]
    fn a_comment_only_edit_passes() {
        let new = edited("//! Module header.\n", "")
            .replace("///\n/// Longer text that a comment edit may cut.\n", "")
            .replace("    // Why the budget is doubled.\n", "")
            .replace(" // trailing note", "")
            .replace("    /* block comment */\n", "")
            .replace(
                "/// A helper.\n",
                "/// A helper, reworded\n/// over two lines.\n",
            );
        assert_eq!(compare("x.rs", BASE, &new), Ok(()));
        assert_eq!(compare("x.rs", BASE, BASE), Ok(()));
    }

    #[test]
    fn a_one_token_code_edit_fails() {
        let new = edited("args.len() * 2", "args.len() * 3");
        let err = compare("x.rs", BASE, &new).unwrap_err();
        assert!(err.contains("`2` became `3`"), "{err}");
        let new = edited(
            "\"a string literal is code\"",
            "\"a string literal is Code\"",
        );
        assert!(compare("x.rs", BASE, &new).is_err());
        let new = edited("#[inline]\n", "");
        assert!(compare("x.rs", BASE, &new).is_err());
    }

    #[test]
    fn a_changed_command_summary_fails() {
        let new = edited(
            "/// Advance virtual time, or wait until a matcher fires.",
            "/// Advance virtual time or wait until a matcher fires.",
        );
        let err = compare("x.rs", BASE, &new).unwrap_err();
        assert!(err.contains("command-summary:"), "{err}");
        // Moving the summary below the attribute or re-indenting it changes no output.
        let moved = edited(
            "/// Advance virtual time, or wait until a matcher fires.\n///\n/// Longer text that a comment edit may cut.\n",
            "",
        )
        .replacen(
            "#[command(name = \"run\", group = \"time\")]\n",
            "#[command(name = \"run\", group = \"time\")]\n///   Advance virtual time, or wait until a matcher fires.\n",
            1,
        );
        assert_eq!(compare("x.rs", BASE, &moved), Ok(()));
    }

    #[test]
    fn a_doc_built_by_a_macro_is_code() {
        let old = "#[doc = concat!(\"a\", \"b\")]\nstruct S;\n";
        let new = "#[doc = concat!(\"a\", \"c\")]\nstruct S;\n";
        assert!(compare("x.rs", old, new).is_err());
    }

    #[test]
    fn a_frozen_marker_must_stay() {
        let path = FROZEN_MARKERS[0].0;
        let old = "mod a {\n    fn f() {}\n    // End of the SetupAPI arm.\n}\n";
        let new = "mod a {\n    fn f() {}\n}\n";
        assert!(compare(path, old, new).unwrap_err().contains("marker"));
        assert_eq!(compare("other.rs", old, new), Ok(()));
    }

    #[test]
    fn string_literals_decode_like_the_command_macro_sees_them() {
        assert_eq!(
            decode_str_literal(r#"" a \"b\" \u{e9}""#).as_deref(),
            Some(" a \"b\" é")
        );
        assert_eq!(decode_str_literal(r##"r#"x"y"#"##).as_deref(), Some("x\"y"));
    }
}
