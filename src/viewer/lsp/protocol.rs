//! The parts of the Language Server Protocol the editor speaks: its JSON
//! types (only the fields read; serde skips the rest), conversions between
//! the editor's char indices and LSP positions, and `file:` URIs.

use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a position's `character` counts: LSP's default is UTF-16 code
/// units, but a server can pick another at start-up. UTF-32 counts chars,
/// as the editor does, so it's offered first.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Encoding {
    Utf8,
    #[default]
    Utf16,
    Utf32,
}

impl Encoding {
    fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    fn units(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub(super) struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Diagnostic {
    pub range: Range,
    /// 1 error, 2 warning, 3 information, 4 hint. Errors when missing.
    pub severity: Option<u8>,
    pub message: String,
    /// What found it, e.g. `rustc` or `Pylance`.
    pub source: Option<String>,
    /// A number or a string, e.g. `E0308`.
    pub code: Option<Value>,
}

impl Diagnostic {
    /// The message as the hover popup shows it: `source(code): message`.
    pub fn text(&self) -> String {
        let code = match &self.code {
            Some(Value::String(code)) => Some(code.clone()),
            Some(Value::Number(code)) => Some(code.to_string()),
            _ => None,
        };
        match (&self.source, code) {
            (Some(source), Some(code)) => format!("{source}({code}): {}", self.message),
            (Some(source), None) => format!("{source}: {}", self.message),
            (None, Some(code)) => format!("{code}: {}", self.message),
            (None, None) => self.message.clone(),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct PublishDiagnostics {
    pub uri: String,
    /// The version of the document they're for, if the server says.
    pub version: Option<i32>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompletionItem {
    pub label: String,
    pub label_details: Option<LabelDetails>,
    /// Shown greyed after the label, e.g. a function's signature.
    pub detail: Option<String>,
    pub sort_text: Option<String>,
    pub filter_text: Option<String>,
    pub insert_text: Option<String>,
    /// 2 for a snippet, with `$1` placeholders.
    pub insert_text_format: Option<u8>,
    pub text_edit: Option<CompletionEdit>,
    /// Edits elsewhere, e.g. an import at the top.
    pub additional_text_edits: Option<Vec<TextEdit>>,
    pub preselect: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct LabelDetails {
    pub detail: Option<String>,
    pub description: Option<String>,
}

/// A completion's `TextEdit`, or its `InsertReplaceEdit`, read as one.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompletionEdit {
    pub new_text: String,
    range: Option<Range>,
    insert: Option<Range>,
}

impl CompletionEdit {
    pub fn range(&self) -> Option<Range> {
        self.range.or(self.insert)
    }
}

impl CompletionItem {
    /// The greyed text after the label.
    pub fn detail(&self) -> Option<&str> {
        let details = self.label_details.as_ref();
        details
            .and_then(|d| d.description.as_deref())
            .or(self.detail.as_deref())
            .or(details.and_then(|d| d.detail.as_deref()))
    }
}

/// Where a definition is.
#[derive(Clone, Debug)]
pub(super) struct Location {
    pub path: PathBuf,
    pub at: Position,
}

/// What a server can do, from its `initialize` reply.
#[derive(Clone, Debug, Default)]
pub(super) struct Capabilities {
    pub encoding: Encoding,
    pub hover: bool,
    pub definition: bool,
    pub formatting: bool,
    /// The characters after which it offers completions (`.`, `:`), if it
    /// completes at all.
    pub completion: Option<Vec<String>>,
}

impl Capabilities {
    pub fn parse(result: &Value) -> Self {
        let caps = &result["capabilities"];
        let provided = |name: &str| {
            !matches!(
                caps.get(name),
                None | Some(Value::Null | Value::Bool(false))
            )
        };
        let completion = provided("completionProvider").then(|| {
            caps["completionProvider"]["triggerCharacters"]
                .as_array()
                .map(|chars| {
                    chars
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        });
        Self {
            encoding: Encoding::from_name(caps["positionEncoding"].as_str()),
            hover: provided("hoverProvider"),
            definition: provided("definitionProvider"),
            formatting: provided("documentFormattingProvider"),
            completion,
        }
    }
}

/// A hover reply's contents as Markdown, or `None` if it has none. LSP
/// allows Markdown, plain text, or the older `MarkedString`s, in a list.
pub(super) fn hover_markdown(result: &Value) -> Option<String> {
    fn marked(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Array(items) => {
                let parts: Vec<String> =
                    items.iter().map(marked).filter(|s| !s.is_empty()).collect();
                parts.join("\n\n---\n\n")
            }
            Value::Object(object) => {
                let text = object
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let kind = object.get("kind").and_then(Value::as_str);
                let language = object.get("language").and_then(Value::as_str);
                match (kind, language) {
                    (Some("markdown"), _) => text.to_owned(),
                    // Plain text, which Markdown would reflow.
                    _ if text.trim().is_empty() => String::new(),
                    _ => format!("```{}\n{text}\n```", language.unwrap_or_default()),
                }
            }
            _ => String::new(),
        }
    }
    let text = marked(result.get("contents")?);
    (!text.trim().is_empty()).then_some(text)
}

/// A definition reply's places: a `Location`, a list of them, or of
/// `LocationLink`s.
pub(super) fn locations(result: Value) -> Vec<Location> {
    let items = match result {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        item => vec![item],
    };
    items
        .iter()
        .filter_map(|item| {
            let (uri, range) = match item.get("targetUri") {
                Some(uri) => (
                    uri,
                    item.get("targetSelectionRange").or(item.get("targetRange")),
                ),
                None => (item.get("uri")?, item.get("range")),
            };
            let range = Range::deserialize(range?).ok()?;
            Some(Location {
                path: uri_to_path(uri.as_str()?)?,
                at: range.start,
            })
        })
        .collect()
}

/// A completion reply's items, and whether typing more should ask again
/// rather than narrow these down. Items that don't parse are left out.
pub(super) fn completions(result: Value) -> (Vec<CompletionItem>, bool) {
    let (items, incomplete) = match result {
        Value::Array(items) => (items, false),
        Value::Object(mut list) => {
            let incomplete = list
                .get("isIncomplete")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match list.get_mut("items").map(Value::take) {
                Some(Value::Array(items)) => (items, incomplete),
                _ => (Vec::new(), incomplete),
            }
        }
        _ => (Vec::new(), false),
    };
    let items = items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect();
    (items, incomplete)
}

/// A formatting reply's edits.
pub(super) fn edits(result: Value) -> Vec<TextEdit> {
    match result {
        Value::Array(edits) => edits
            .into_iter()
            .filter_map(|edit| serde_json::from_value(edit).ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Where each line of a text starts, in bytes and in chars, for converting
/// between the editor's char indices and LSP positions.
pub(super) struct Lines {
    bytes: Vec<usize>,
    chars: Vec<usize>,
    len: usize,
}

impl Lines {
    pub fn new(text: &str) -> Self {
        let mut bytes = vec![0];
        let mut chars = vec![0];
        let mut len = 0;
        for (at, c) in text.char_indices() {
            len += 1;
            if c == '\n' {
                bytes.push(at + 1);
                chars.push(len);
            }
        }
        Self { bytes, chars, len }
    }

    pub fn count(&self) -> usize {
        self.bytes.len()
    }

    /// The line char index `at` is on, from 0.
    pub fn line_of(&self, at: usize) -> usize {
        self.chars.partition_point(|&start| start <= at) - 1
    }

    /// The char index of `pos` in `text`, which these lines are of: clamped
    /// to the end of its line, and of the text.
    pub fn char_index(&self, text: &str, pos: Position, encoding: Encoding) -> usize {
        let line = pos.line as usize;
        let Some(&start) = self.bytes.get(line) else {
            return self.len;
        };
        let end = self
            .bytes
            .get(line + 1)
            .map_or(text.len(), |&next| next - 1);
        let (mut units, mut column) = (0, 0);
        for c in text[start..end].chars() {
            if units >= pos.character as usize {
                break;
            }
            units += encoding.units(c);
            column += 1;
        }
        self.chars[line] + column
    }

    /// The position of char index `at` of `text`, which these lines are of.
    pub fn position(&self, text: &str, at: usize, encoding: Encoding) -> Position {
        let at = at.min(self.len);
        let line = self.line_of(at);
        let column = at - self.chars[line];
        let character = match encoding {
            Encoding::Utf32 => column,
            _ => text[self.bytes[line]..]
                .chars()
                .take(column)
                .map(|c| encoding.units(c))
                .sum(),
        };
        Position {
            line: line as u32,
            character: character as u32,
        }
    }
}

/// The `file:` URI of `path`: `file:///C:/a%20b.rs` on Windows.
pub(super) fn path_to_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = match path.strip_prefix("//") {
        // A UNC path's server is the URI's host.
        Some(unc) => unc.to_owned(),
        None if path.starts_with('/') => path,
        None => format!("/{path}"),
    };
    let mut uri = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            uri.push(b as char);
        } else {
            let _ = write!(uri, "%{b:02X}");
        }
    }
    uri
}

/// The path of a `file:` URI, however the server spelled it (VS Code's
/// `file:///c%3A/...` as well as `file:///C:/...`).
pub(super) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut iter = rest.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    if cfg!(windows) {
        let path = match path.strip_prefix('/') {
            // `/C:/...`
            Some(local) if local.as_bytes().get(1) == Some(&b':') => local.to_owned(),
            Some(local) => format!("/{local}"),
            // A host: a UNC path.
            None => format!("//{path}"),
        };
        Some(PathBuf::from(path.replace('/', "\\")))
    } else {
        // `file://localhost/...` names this machine.
        let path = path.strip_prefix("localhost").unwrap_or(&path);
        Some(PathBuf::from(path))
    }
}

/// `path` as a key that matches however a server spells it: Windows paths
/// ignore case.
pub(super) fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy();
    if cfg!(windows) {
        key.replace('/', "\\").to_lowercase()
    } else {
        key.into_owned()
    }
}

/// A snippet (`$1`, `${2:name}`, `${3|a,b|}`, `$0`) as plain text, with
/// each placeholder's default, and the chars of it to select: the first
/// tab stop's placeholder (empty for a bare `$1`), else where `$0` is.
/// `None` if it has neither: the caret goes at the end.
pub(super) fn strip_snippet(snippet: &str) -> (String, Option<std::ops::Range<usize>>) {
    fn number(chars: &mut std::iter::Peekable<std::str::Chars>) -> u32 {
        let mut n = 0;
        while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
            n = n * 10 + d;
            chars.next();
        }
        n
    }
    let mut text = String::new();
    let mut len = 0;
    // Each tab stop's number, and the chars of its placeholder.
    let mut stops: Vec<(u32, std::ops::Range<usize>)> = Vec::new();
    // The `${n:...}`s open here, innermost last, by index in `stops`.
    let mut open: Vec<usize> = Vec::new();
    let mut chars = snippet.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    text.push(next);
                    len += 1;
                }
            }
            '$' if chars.peek().is_some_and(char::is_ascii_digit) => {
                stops.push((number(&mut chars), len..len));
            }
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                stops.push((number(&mut chars), len..len));
                match chars.next() {
                    Some(':') => open.push(stops.len() - 1),
                    // A choice: the first one.
                    Some('|') => {
                        let choice: String = chars
                            .by_ref()
                            .take_while(|&c| c != ',' && c != '|')
                            .collect();
                        len += choice.chars().count();
                        text.push_str(&choice);
                        if let Some((_, range)) = stops.last_mut() {
                            range.end = len;
                        }
                        for c in chars.by_ref() {
                            if c == '}' {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            '}' if !open.is_empty() => {
                if let Some(i) = open.pop() {
                    stops[i].1.end = len;
                }
            }
            c => {
                text.push(c);
                len += 1;
            }
        }
    }
    let first = stops.iter().filter(|(n, _)| *n > 0).min_by_key(|(n, _)| *n);
    let caret = first
        .or(stops.iter().find(|(n, _)| *n == 0))
        .map(|(_, range)| range.clone());
    (text, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_in_each_encoding() {
        // `é` is 2 UTF-8 bytes, `𝄞` 4 bytes and 2 UTF-16 units.
        let text = "ab\né𝄞x\n";
        let lines = Lines::new(text);
        assert_eq!(lines.count(), 3);
        let x = 3 + 2;
        for (encoding, character) in [
            (Encoding::Utf32, 2),
            (Encoding::Utf16, 3),
            (Encoding::Utf8, 6),
        ] {
            let pos = lines.position(text, x, encoding);
            assert_eq!(pos, Position { line: 1, character });
            assert_eq!(lines.char_index(text, pos, encoding), x);
        }
        // Past the end of a line, or of the text.
        let pos = Position {
            line: 0,
            character: 99,
        };
        assert_eq!(lines.char_index(text, pos, Encoding::Utf16), 2);
        let pos = Position {
            line: 9,
            character: 0,
        };
        assert_eq!(
            lines.char_index(text, pos, Encoding::Utf16),
            text.chars().count()
        );
    }

    #[test]
    fn uris_round_trip() {
        let path = if cfg!(windows) {
            r"C:\Users\me\a b#c.rs"
        } else {
            "/home/me/a b#c.rs"
        };
        let uri = path_to_uri(Path::new(path));
        assert!(uri.starts_with("file:///"));
        assert!(uri.ends_with("a%20b%23c.rs"));
        assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from(path));
        if cfg!(windows) {
            // VS Code's spelling, as some servers echo it.
            let path = uri_to_path("file:///c%3A/Users/me/x.rs").unwrap();
            assert_eq!(path_key(&path), path_key(Path::new(r"C:\Users\me\x.rs")));
            let unc = Path::new(r"\\server\share\x.py");
            assert_eq!(uri_to_path(&path_to_uri(unc)).unwrap(), unc);
        }
    }

    #[test]
    fn snippets_become_plain_text() {
        assert_eq!(strip_snippet("foo($1)$0"), ("foo()".into(), Some(4..4)));
        assert_eq!(strip_snippet("len()$0"), ("len()".into(), Some(5..5)));
        assert_eq!(
            strip_snippet("push(${1:value})$0"),
            ("push(value)".into(), Some(5..10))
        );
        assert_eq!(
            strip_snippet("for ${1:x} in ${2:xs} {\n\t$0\n}"),
            ("for x in xs {\n\t\n}".into(), Some(4..5))
        );
        assert_eq!(strip_snippet("${1:a${2:b}c}d"), ("abcd".into(), Some(0..3)));
        assert_eq!(
            strip_snippet("${1|a,b|} \\$x ${2}"),
            ("a $x ".into(), Some(0..1))
        );
        assert_eq!(strip_snippet("plain"), ("plain".into(), None));
    }

    #[test]
    fn replies_parse() {
        let hover = serde_json::json!({"contents": {"kind": "markdown", "value": "**x**"}});
        assert_eq!(hover_markdown(&hover).as_deref(), Some("**x**"));
        let hover =
            serde_json::json!({"contents": [{"language": "python", "value": "x: int"}, "doc"]});
        assert_eq!(
            hover_markdown(&hover).as_deref(),
            Some("```python\nx: int\n```\n\n---\n\ndoc")
        );
        assert_eq!(hover_markdown(&serde_json::json!({"contents": ""})), None);

        let uri = path_to_uri(Path::new(if cfg!(windows) { r"C:\a.rs" } else { "/a.rs" }));
        let range = serde_json::json!({"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 5}});
        let link = serde_json::json!([{"targetUri": uri, "targetRange": range, "targetSelectionRange": range}]);
        let found = locations(link);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].at,
            Position {
                line: 3,
                character: 4
            }
        );
        assert_eq!(
            locations(serde_json::json!({"uri": uri, "range": range})).len(),
            1
        );

        let list = serde_json::json!({"isIncomplete": true, "items": [{"label": "a"}, {"label": 3}, {"label": "b", "textEdit": {"newText": "b", "range": range}}]});
        let (items, incomplete) = completions(list);
        assert!(incomplete);
        assert_eq!(items.len(), 2);
        assert!(items[1].text_edit.as_ref().unwrap().range().is_some());

        let caps = Capabilities::parse(&serde_json::json!({"capabilities": {
            "positionEncoding": "utf-32", "hoverProvider": true, "definitionProvider": {},
            "documentFormattingProvider": false, "completionProvider": {"triggerCharacters": [".", ":"]}
        }}));
        assert_eq!(caps.encoding, Encoding::Utf32);
        assert!(caps.hover && caps.definition && !caps.formatting);
        assert_eq!(caps.completion.unwrap(), [".", ":"]);
    }
}
