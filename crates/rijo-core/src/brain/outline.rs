//! Symbol outlines for code, markup and stylesheets.
//!
//! The brain indexes *chunks*; this module extracts the structure above them:
//! function/class/section names with their line and signature. Two consumers:
//! the `brain_outline` tool (map a file or directory without reading bodies) and
//! the indexer (symbol nodes, so `brain_search` can answer "where is X defined").
//!
//! Heuristic on purpose: no parser dependency, works on partially written files,
//! and every language degrades to "no symbols" rather than failing a read.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// fn | struct | enum | trait | impl | mod | class | function | const |
    /// section | form | heading | …
    pub kind: String,
    /// The declaration line, trimmed and capped.
    pub signature: String,
    pub line: usize,
}

const MAX_SIGNATURE: usize = 160;

fn push(symbols: &mut Vec<Symbol>, name: &str, kind: &str, line: usize, signature: &str) {
    if name.is_empty() || symbols.len() >= 2_000 {
        return;
    }
    let signature: String = signature.trim().chars().take(MAX_SIGNATURE).collect();
    symbols.push(Symbol {
        name: name.to_string(),
        kind: kind.to_string(),
        signature,
        line,
    });
}

fn ident_after(text: &str) -> &str {
    text.trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '!'))
        .next()
        .unwrap_or("")
}

/// Extract the outline of one file. Unknown extensions return an empty list.
pub fn extract(path: &Path, content: &str) -> Vec<Symbol> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => rust(content),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => javascript(content),
        "py" => python(content),
        "html" | "htm" | "svg" => html(content),
        "md" | "markdown" => markdown(content),
        _ => Vec::new(),
    }
}

fn rust(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim_start();
        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with("/*")
            || line.starts_with('*')
            || line.starts_with('#')
        {
            continue;
        }
        let mut rest = line;
        for prefix in ["pub(crate) ", "pub(super) ", "pub ", "pub(in crate) "] {
            if let Some(stripped) = rest.strip_prefix(prefix) {
                rest = stripped;
                break;
            }
        }
        for prefix in ["async ", "unsafe ", "const "] {
            if let Some(stripped) = rest.strip_prefix(prefix) {
                rest = stripped;
            }
        }
        let mut matched = false;
        for (keyword, kind) in [
            ("fn ", "fn"),
            ("struct ", "struct"),
            ("enum ", "enum"),
            ("trait ", "trait"),
            ("mod ", "mod"),
            ("type ", "type"),
            ("static ", "static"),
            ("macro_rules! ", "macro"),
        ] {
            if let Some(after) = rest.strip_prefix(keyword) {
                let name = ident_after(after);
                push(&mut symbols, name, kind, index + 1, line);
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if let Some(after) = rest.strip_prefix("impl") {
            // `impl Foo`, `impl<T> Foo<T>`, `impl Trait for Foo`
            let body = after.trim_start_matches('<');
            let body = if after.starts_with('<') {
                // skip the generic parameter list
                match body.find('>') {
                    Some(close) => &body[close + 1..],
                    None => body,
                }
            } else {
                body
            };
            let target = body.split(" for ").last().unwrap_or(body).trim();
            let name = ident_after(target);
            push(&mut symbols, name, "impl", index + 1, line);
        }
    }
    symbols
}

fn javascript(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with("//") || line.starts_with('*') {
            continue;
        }
        let top_level = !raw.starts_with([' ', '\t']);
        let rest = line
            .strip_prefix("export default ")
            .or_else(|| line.strip_prefix("export "))
            .unwrap_or(line);
        let mut handled = false;
        for (keyword, kind) in [
            ("async function ", "function"),
            ("function ", "function"),
            ("class ", "class"),
            ("const ", "const"),
            ("let ", "let"),
            ("var ", "var"),
        ] {
            if let Some(after) = rest.strip_prefix(keyword) {
                let name = ident_after(after);
                if name.is_empty() || name == "(" {
                    continue;
                }
                // For variable declarations only report things that look like a
                // function or object (a plain `const x = 3` is not structure),
                // and only at top level: locals are not part of the outline.
                if matches!(keyword, "const " | "let " | "var ") {
                    if !top_level {
                        continue;
                    }
                    let tail = &after[after.find(name).map(|p| p + name.len()).unwrap_or(0)..];
                    let tail = tail.trim_start();
                    if !(tail.starts_with('=')
                        && (tail.contains("=>") || tail.contains("function")))
                    {
                        continue;
                    }
                }
                push(&mut symbols, name, kind, index + 1, line);
                handled = true;
                break;
            }
        }
        if handled {
            continue;
        }
        if line.starts_with('@') {
            // decorators are noise for an outline
            continue;
        }
    }
    symbols
}

fn python(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim_start();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let rest = line.strip_prefix("async ").unwrap_or(line);
        for (keyword, kind) in [("def ", "def"), ("class ", "class")] {
            if let Some(after) = rest.strip_prefix(keyword) {
                push(&mut symbols, ident_after(after), kind, index + 1, line);
            }
        }
    }
    symbols
}

fn attribute(text: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn html(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim_start();
        if !line.starts_with('<') {
            continue;
        }
        for (tag, kind) in [
            ("section", "section"),
            ("form", "form"),
            ("dialog", "dialog"),
            ("aside", "aside"),
            ("nav", "nav"),
            ("header", "header"),
            ("footer", "footer"),
            ("main", "main"),
            ("h1", "heading"),
            ("h2", "heading"),
        ] {
            let open = format!("<{tag}");
            if !line.starts_with(&open) {
                continue;
            }
            let after = &line[open.len()..];
            let next = after.chars().next().unwrap_or(' ');
            if !(next.is_whitespace() || next == '>') {
                continue;
            }
            let name = attribute(line, "id")
                .or_else(|| attribute(line, "class"))
                .unwrap_or_else(|| {
                    let text = strip_tags(line.split_once('>').map(|(_, rest)| rest).unwrap_or(""));
                    if text.is_empty() {
                        tag.to_string()
                    } else {
                        text.chars().take(48).collect()
                    }
                });
            push(&mut symbols, &name, kind, index + 1, line);
            break;
        }
    }
    symbols
}

fn markdown(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim_start();
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=3).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
            let name = line[hashes..].trim();
            push(&mut symbols, name, "heading", index + 1, line);
        }
    }
    symbols
}

#[cfg(test)]
mod tests {
    use super::extract;
    use std::path::Path;

    #[test]
    fn rust_symbols_include_visibility_and_impl_targets() {
        let src = r#"
pub struct Cart { items: Vec<Item> }
impl Cart {
    pub fn add(&mut self, item: Item) {}
}
pub(crate) async fn render_cart() {}
fn private_helper() {}
"#;
        let symbols = extract(Path::new("cart.rs"), src);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Cart"), "{names:?}");
        assert!(names.contains(&"add"), "{names:?}");
        assert!(names.contains(&"render_cart"), "{names:?}");
        assert!(names.contains(&"private_helper"), "{names:?}");
        let add = symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.kind, "fn");
        assert_eq!(add.line, 4);
    }

    #[test]
    fn javascript_symbols_cover_functions_classes_and_arrow_consts() {
        let src = r#"
export function renderGrid(items) {}
const addToCart = (id) => {};
const answer = 42;
class Drawer {}
async function openCart() {}
"#;
        let symbols = extract(Path::new("main.js"), src);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"renderGrid"), "{names:?}");
        assert!(names.contains(&"addToCart"), "{names:?}");
        assert!(names.contains(&"Drawer"), "{names:?}");
        assert!(names.contains(&"openCart"), "{names:?}");
        assert!(!names.contains(&"answer"), "plain consts are not structure");
    }

    #[test]
    fn html_symbols_use_ids_and_headings() {
        let src = r#"
<section id="bestseller" class="section">
<h2>Bestseller</h2>
<form id="contactForm" novalidate>
"#;
        let symbols = extract(Path::new("index.html"), src);
        assert!(symbols
            .iter()
            .any(|s| s.name == "bestseller" && s.kind == "section"));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Bestseller" && s.kind == "heading"));
        assert!(symbols
            .iter()
            .any(|s| s.name == "contactForm" && s.kind == "form"));
    }

    #[test]
    fn unknown_files_have_no_outline() {
        assert!(extract(Path::new("data.bin"), "whatever").is_empty());
    }
}
