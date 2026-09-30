/*
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Stalwart's vendor Sieve names, renamed for inbuxa.
//!
//! inbuxa accepts `vnd.inbuxa.while` and `vnd.inbuxa.expressions` where
//! Stalwart accepted `vnd.stalwart.*`, with no alias, and names its
//! environment items the same way. A script carried over unchanged fails to
//! compile on inbuxa, so export renames those names -- and only those names:
//! the strings of a `require` list, the name argument of an `environment`
//! test, and `${env.vnd.stalwart.…}` references inside strings. Everything
//! else in the script, including other strings that happen to contain the
//! text, is copied byte for byte.

const OLD: &str = "vnd.stalwart.";
const NEW: &str = "vnd.inbuxa.";
const OLD_ENV_REF: &str = "${env.vnd.stalwart.";
const NEW_ENV_REF: &str = "${env.vnd.inbuxa.";

/// Whether the target advertises inbuxa's vendor extensions, from the
/// `sieveExtensions` list of its `urn:ietf:params:jmap:sieve` account
/// capability.
pub fn target_uses_inbuxa_names(sieve_extensions: &[String]) -> bool {
    sieve_extensions.iter().any(|e| e.starts_with(NEW))
}

/// The script with Stalwart's vendor names renamed, and the old names that
/// were changed, in order. `None` when nothing needed renaming, or when the
/// script is not UTF-8 (left alone rather than guessed at).
pub fn rewrite(script: &[u8]) -> Option<(Vec<u8>, Vec<String>)> {
    let text = std::str::from_utf8(script).ok()?;
    if !text.contains(OLD) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut renamed = Vec::new();
    let mut copied = 0;
    let mut context = Context::None;
    for tok in Tokens::new(text) {
        match tok.kind {
            Kind::Word => {
                context = match tok.text(text).to_ascii_lowercase().as_str() {
                    "require" => Context::Require,
                    "environment" => Context::Environment,
                    _ if context == Context::Environment => Context::Environment,
                    _ => Context::None,
                };
            }
            Kind::Tag => {
                // `environment :comparator "i;octet"`: the comparator's own
                // string is not the item name.
                if context == Context::Environment
                    && tok.text(text).eq_ignore_ascii_case(":comparator")
                {
                    context = Context::EnvironmentComparator;
                }
            }
            Kind::Quoted | Kind::Multiline => {
                let (start, end) = tok.content;
                let content = &text[start..end];
                let mut replacement: Option<String> = None;
                let whole_name = matches!(context, Context::Require | Context::Environment);
                if whole_name && content.starts_with(OLD) {
                    replacement = Some(format!("{NEW}{}", &content[OLD.len()..]));
                    renamed.push(content.to_owned());
                }
                let current = replacement.as_deref().unwrap_or(content);
                if current.contains(OLD_ENV_REF) {
                    let mut n = 0;
                    let mut rest = current;
                    while let Some(i) = rest.find(OLD_ENV_REF) {
                        let tail = &rest[i + 2..];
                        let name_end = tail.find('}').unwrap_or(tail.len());
                        renamed.push(tail[4..name_end].to_owned());
                        rest = &rest[i + OLD_ENV_REF.len()..];
                        n += 1;
                    }
                    if n > 0 {
                        replacement = Some(current.replace(OLD_ENV_REF, NEW_ENV_REF));
                    }
                }
                if let Some(r) = replacement {
                    out.push_str(&text[copied..start]);
                    out.push_str(&r);
                    copied = end;
                }
                context = match context {
                    Context::Require => Context::Require,
                    Context::EnvironmentComparator => Context::Environment,
                    _ => Context::None,
                };
            }
            Kind::Punct(';') | Kind::Punct('{') | Kind::Punct('}') => context = Context::None,
            Kind::Punct(_) => {}
        }
    }
    if renamed.is_empty() {
        return None;
    }
    out.push_str(&text[copied..]);
    Some((out.into_bytes(), renamed))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    None,
    Require,
    Environment,
    EnvironmentComparator,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Tag,
    Quoted,
    Multiline,
    Punct(char),
}

struct Token {
    kind: Kind,
    span: (usize, usize),
    /// The string's content, without quotes or the `text:` framing.
    content: (usize, usize),
}

impl Token {
    fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.span.0..self.span.1]
    }
}

/// Just enough of RFC 5228's lexer to find strings and the words before
/// them: comments are skipped, and quoted strings and `text:` blocks are
/// read whole, so nothing inside them is mistaken for a command.
struct Tokens<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Tokens<'a> {
    fn new(src: &'a str) -> Self {
        Tokens { src, pos: 0 }
    }
}

impl Iterator for Tokens<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        let b = self.src.as_bytes();
        loop {
            while self.pos < b.len() && b[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.pos >= b.len() {
                return None;
            }
            if b[self.pos] == b'#' {
                while self.pos < b.len() && b[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            if b[self.pos..].starts_with(b"/*") {
                self.pos = match self.src[self.pos + 2..].find("*/") {
                    Some(i) => self.pos + 2 + i + 2,
                    None => b.len(),
                };
                continue;
            }
            break;
        }
        let start = self.pos;
        let c = b[start];
        if c == b'"' {
            let mut i = start + 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            let end = i.min(b.len());
            self.pos = (end + 1).min(b.len());
            return Some(Token {
                kind: Kind::Quoted,
                span: (start, self.pos),
                content: (start + 1, end),
            });
        }
        if c.is_ascii_alphabetic() || c == b'_' || c == b':' {
            let mut i = start + 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &self.src[start..i];
            if word.eq_ignore_ascii_case("text:")
                || (word.eq_ignore_ascii_case("text") && b.get(i) == Some(&b':'))
            {
                let after = if b.get(i) == Some(&b':') { i + 1 } else { i };
                // The body starts after the rest of the `text:` line and runs
                // to a line holding a single dot.
                let body = match self.src[after..].find('\n') {
                    Some(n) => after + n + 1,
                    None => b.len(),
                };
                let (body_end, next) = find_dot_line(self.src, body);
                self.pos = next;
                return Some(Token {
                    kind: Kind::Multiline,
                    span: (start, next),
                    content: (body, body_end),
                });
            }
            self.pos = i;
            let kind = if c == b':' { Kind::Tag } else { Kind::Word };
            return Some(Token {
                kind,
                span: (start, i),
                content: (start, i),
            });
        }
        let ch = self.src[start..].chars().next().unwrap_or('\0');
        self.pos = start + ch.len_utf8();
        Some(Token {
            kind: Kind::Punct(ch),
            span: (start, self.pos),
            content: (start, self.pos),
        })
    }
}

/// End of a `text:` body (the start of its closing dot line) and the offset
/// after that line.
fn find_dot_line(src: &str, from: usize) -> (usize, usize) {
    let mut line_start = from;
    while line_start < src.len() {
        let line_end = src[line_start..]
            .find('\n')
            .map(|n| line_start + n)
            .unwrap_or(src.len());
        if src[line_start..line_end].trim_end_matches('\r') == "." {
            return (line_start, (line_end + 1).min(src.len()));
        }
        line_start = line_end + 1;
    }
    (src.len(), src.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str) -> Option<(String, Vec<String>)> {
        rewrite(s.as_bytes()).map(|(b, r)| (String::from_utf8(b).unwrap(), r))
    }

    #[test]
    fn renames_a_require_list() {
        let (out, renamed) =
            run("require [\"fileinto\", \"vnd.stalwart.while\", \"vnd.stalwart.expressions\"];\n")
                .unwrap();
        assert_eq!(
            out,
            "require [\"fileinto\", \"vnd.inbuxa.while\", \"vnd.inbuxa.expressions\"];\n"
        );
        assert_eq!(renamed, ["vnd.stalwart.while", "vnd.stalwart.expressions"]);
    }

    #[test]
    fn renames_a_single_require_string() {
        let (out, _) = run("REQUIRE \"vnd.stalwart.while\";").unwrap();
        assert_eq!(out, "REQUIRE \"vnd.inbuxa.while\";");
    }

    #[test]
    fn renames_the_environment_item_name_only() {
        let src = "if environment :comparator \"i;octet\" :is \"vnd.stalwart.username\" \"vnd.stalwart.x\" { keep; }";
        let (out, renamed) = run(src).unwrap();
        assert_eq!(
            out,
            "if environment :comparator \"i;octet\" :is \"vnd.inbuxa.username\" \"vnd.stalwart.x\" { keep; }"
        );
        assert_eq!(renamed, ["vnd.stalwart.username"]);
    }

    #[test]
    fn renames_env_references_inside_strings() {
        let src = "set \"box\" \"${env.vnd.stalwart.default_mailbox}/Archive\";";
        let (out, renamed) = run(src).unwrap();
        assert_eq!(
            out,
            "set \"box\" \"${env.vnd.inbuxa.default_mailbox}/Archive\";"
        );
        assert_eq!(renamed, ["vnd.stalwart.default_mailbox"]);
    }

    #[test]
    fn renames_env_references_in_text_blocks() {
        let src = "vacation text:\nHi ${env.vnd.stalwart.username}.\n.\n;\n";
        let (out, _) = run(src).unwrap();
        assert_eq!(
            out,
            "vacation text:\nHi ${env.vnd.inbuxa.username}.\n.\n;\n"
        );
    }

    #[test]
    fn leaves_unrelated_strings_comments_and_text_alone() {
        let src = "# vnd.stalwart.while is old\n/* \"vnd.stalwart.x\" */\n\
                   if header :contains \"subject\" \"vnd.stalwart.while\" { fileinto \"vnd.stalwart.box\"; }\n\
                   vacation text:\nrequire \"vnd.stalwart.while\";\n.\n;\n";
        assert!(run(src).is_none());
    }

    #[test]
    fn require_context_ends_at_the_semicolon() {
        let src = "require \"fileinto\"; fileinto \"vnd.stalwart.folder\";";
        assert!(run(src).is_none());
    }

    #[test]
    fn nothing_to_do_is_none() {
        assert!(run("require \"fileinto\";\nkeep;\n").is_none());
        assert!(rewrite(&[0xff, 0xfe, b'v']).is_none());
    }

    #[test]
    fn target_detection() {
        assert!(target_uses_inbuxa_names(&[
            "fileinto".to_owned(),
            "vnd.inbuxa.while".to_owned()
        ]));
        assert!(!target_uses_inbuxa_names(&[
            "fileinto".to_owned(),
            "vnd.stalwart.while".to_owned()
        ]));
        assert!(!target_uses_inbuxa_names(&[]));
    }
}
