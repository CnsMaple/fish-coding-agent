use crate::theme::Theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Tag};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::sync::Mutex;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Inline style applier: takes raw text and returns a styled `Span`.
type InlineStyleFn = Box<dyn Fn(&str) -> Span<'static>>;

/// Raw-Markdown table → formatted table block renderer.
/// Works on any pipe-delimited block regardless of line endings,
/// separators, or surrounding content, so it handles the incomplete
/// / unusual output that LLMs often produce.  After this pre-pass
/// the result is fed to pulldown-cmark for full CommonMark rendering.
fn preprocess_tables(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let is_table_row = trimmed.starts_with('|') && trimmed.ends_with('|');
        if is_table_row {
            if !in_table {
                out.push('\n');
                in_table = true;
            }
            out.push_str(trimmed);
        } else {
            in_table = false;
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Render Markdown text into styled TUI lines.
/// Line endings are normalized to `\n` before parsing so that CRLF / bare
/// CR from API responses don't fool the table parser.
/// Invisible Unicode artifacts (BOM, zero-width chars) are also stripped.
pub fn render(text: &str) -> Vec<Line<'static>> {
    render_with_width(text, usize::MAX / 2)
}

pub fn render_with_width(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut cleaned = text.to_string();
    // 1. Normalize line endings.
    if cleaned.contains('\r') {
        cleaned = cleaned.replace("\r\n", "\n").replace('\r', "\n");
    }
    // 2. Strip zero-width / invisible chars that can break table detection.
    cleaned = cleaned
        .chars()
        .filter(|&c| {
            !matches!(
                c,
                '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' |  // ZWJ, ZWNJ, BOM
            '\u{00AD}' | '\u{2060}' | '\u{2061}' | '\u{2062}' |
            '\u{2063}' | '\u{2064}'
            )
        })
        .collect();
    // 3. Pre-process table-like blocks so even concatenated single-line
    //    tables or oddly-formatted LLM output gets proper line breaks.
    cleaned = preprocess_tables(&cleaned);
    MdRenderer::new(width).render(&cleaned)
}

/// Minimum-viable Markdown → ratatui renderer.  Covers the most common
/// constructs: paragraphs, headings, code blocks, lists, bold, italic,
/// inline code, and links.  Everything else is rendered as plain text.
struct MdRenderer {
    /// Current spans for the line being built.
    spans: Vec<Span<'static>>,
    /// Accumulated output lines.
    out: Vec<Line<'static>>,
    /// Inline style stack — pushed on `Start(Tag::Emphasis)` etc., popped
    /// on `End`.
    style_stack: Vec<InlineStyleFn>,
    table: Option<TableState>,
    code_block: Option<CodeBlockState>,
    list_stack: Vec<ListState>,
    /// True while we are inside a table cell.
    in_table_cell: bool,
    /// True while we are inside a table header row.
    in_table_head: bool,
    max_width: usize,
}

type TableCell = Vec<Span<'static>>;
type TableRow = Vec<TableCell>;

struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: TableRow,
    current_cell: TableCell,
    header_rows: usize,
}

struct ListState {
    next: usize,
    ordered: bool,
}

struct CodeBlockState {
    lang: Option<String>,
    text: String,
}

impl MdRenderer {
    fn new(max_width: usize) -> Self {
        Self {
            spans: Vec::new(),
            out: Vec::new(),
            style_stack: Vec::new(),
            table: None,
            code_block: None,
            list_stack: Vec::new(),
            in_table_cell: false,
            in_table_head: false,
            max_width,
        }
    }

    fn flush_line(&mut self) {
        if !self.spans.is_empty() {
            self.out.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    fn text_span(&self, t: &str) -> Span<'static> {
        if self.style_stack.is_empty() {
            Span::raw(t.to_string())
        } else {
            let s = t.to_string();
            let mut base = Span::raw(s.clone());
            for f in &self.style_stack {
                base = f(&s);
            }
            base
        }
    }

    fn push_text(&mut self, t: &str) {
        if let Some(code) = self.code_block.as_mut() {
            code.text.push_str(t);
            return;
        }
        if self.in_table_cell && self.table.is_some() {
            let span = self.text_span(t);
            if let Some(table) = self.table.as_mut() {
                table.current_cell.push(span);
            }
            return;
        }
        // If there are active inline styles, apply them; otherwise plain.
        self.spans.push(self.text_span(t));
    }

    fn push_code(&mut self, t: &str) {
        // Strip the markdown backticks from the rendered output: the
        // backticks are syntax, not content. Blue keeps inline code
        // distinct while inheriting the terminal theme and avoiding a
        // background/reversed highlight.
        let span = Span::styled(format!(" {t} "), Style::default().fg(Color::Blue));
        if self.in_table_cell && self.table.is_some() {
            if let Some(table) = self.table.as_mut() {
                table.current_cell.push(span);
            }
            return;
        }
        self.spans.push(span);
    }

    fn push_table_break(&mut self) {
        if let Some(code) = self.code_block.as_mut() {
            code.text.push('\n');
            return;
        }
        if self.in_table_cell && self.table.is_some() {
            if let Some(table) = self.table.as_mut() {
                table.current_cell.push(Span::raw(" "));
            }
            return;
        }
        self.flush_line();
    }

    fn push_code_block(&mut self, code: CodeBlockState) {
        let mut raw_lines: Vec<&str> = code.text.lines().collect();
        while raw_lines
            .last()
            .map(|line| line.is_empty())
            .unwrap_or(false)
        {
            raw_lines.pop();
        }

        let content_width = raw_lines
            .iter()
            .map(|line| UnicodeWidthStr::width(*line))
            .max()
            .unwrap_or(0)
            .max(
                code.lang
                    .as_deref()
                    .map(UnicodeWidthStr::width)
                    .unwrap_or(0),
            )
            .max(16);
        let max_content_width = self.max_width.saturating_sub(4).max(16);
        let width = content_width.min(max_content_width);

        let title = code.lang.as_deref().unwrap_or("code");
        let title_width = UnicodeWidthStr::width(title);
        let top = if title_width <= width {
            label_border(title, width)
        } else {
            table_border('+', '+', '+', &[width])
        };
        self.out.push(Line::from(Span::styled(top, Theme::dim())));

        if raw_lines.is_empty() {
            self.out.push(code_line(Vec::new(), width));
        } else {
            for highlighted in highlight_code_lines(&raw_lines, code.lang.as_deref()) {
                for wrapped in wrap_cell(&highlighted, width) {
                    self.out.push(code_line(wrapped, width));
                }
            }
        }

        self.out.push(Line::from(Span::styled(
            table_border('+', '+', '+', &[width]),
            Theme::dim(),
        )));
    }

    fn push_list_marker(&mut self) {
        self.flush_line();
        let depth = self.list_stack.len().saturating_sub(1);
        self.spans.push(Span::raw("  ".repeat(depth).to_string()));
        if let Some(list) = self.list_stack.last_mut() {
            if list.ordered {
                let n = list.next;
                list.next += 1;
                self.spans.push(Span::raw(format!("{n}. ")));
            } else {
                self.spans.push(Span::raw("• "));
            }
        }
    }

    fn push_table(&mut self, table: TableState) {
        let col_count = table
            .rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(table.alignments.len());
        if col_count == 0 {
            return;
        }

        let mut widths = vec![1usize; col_count];
        for row in &table.rows {
            for (idx, cell) in row.iter().enumerate() {
                widths[idx] = widths[idx].max(cell_width(cell));
            }
        }
        fit_table_width(&mut widths, self.max_width);

        self.out.push(Line::from(Span::styled(
            table_border('+', '+', '+', &widths),
            Theme::dim(),
        )));
        for (idx, row) in table.rows.iter().enumerate() {
            self.out
                .extend(table_row_lines(row, &widths, &table.alignments));
            if table.header_rows > 0 && idx + 1 == table.header_rows && idx + 1 < table.rows.len() {
                self.out.push(Line::from(Span::styled(
                    table_border('+', '+', '+', &widths),
                    Theme::dim(),
                )));
            }
        }
        self.out.push(Line::from(Span::styled(
            table_border('+', '+', '+', &widths),
            Theme::dim(),
        )));
    }

    fn render(&mut self, text: &str) -> Vec<Line<'static>> {
        use pulldown_cmark::TagEnd;
        let parser = pulldown_cmark::Parser::new_ext(
            text,
            Options::ENABLE_TABLES
                | Options::ENABLE_STRIKETHROUGH
                | Options::ENABLE_TASKLISTS
                | Options::ENABLE_FOOTNOTES,
        );
        for event in parser {
            match event {
                Event::Text(t) => self.push_text(t.as_ref()),
                Event::Code(t) => self.push_code(t.as_ref()),
                Event::Html(t) => self.push_text(t.as_ref()),
                Event::Rule => self.out.push(Line::from(Span::styled(
                    "─".repeat(self.max_width.clamp(8, 80)),
                    Theme::dim(),
                ))),
                Event::TaskListMarker(checked) => {
                    self.push_text(if checked { "[x] " } else { "[ ] " });
                }
                Event::SoftBreak | Event::HardBreak => self.push_table_break(),
                Event::Start(tag) => match tag {
                    Tag::Heading { .. } => {
                        self.style_stack
                            .push(Box::new(|s| Span::styled(s.to_string(), Theme::bold())));
                    }
                    Tag::Emphasis => {
                        self.style_stack.push(Box::new(|s| {
                            Span::styled(
                                s.to_string(),
                                Theme::dim().add_modifier(ratatui::style::Modifier::ITALIC),
                            )
                        }));
                    }
                    Tag::Strong => {
                        self.style_stack
                            .push(Box::new(|s| Span::styled(s.to_string(), Theme::bold())));
                    }
                    Tag::Link { .. } => {
                        self.style_stack.push(Box::new(|s| {
                            Span::styled(s.to_string(), Theme::underlined())
                        }));
                    }
                    Tag::BlockQuote(_) => {
                        self.flush_line();
                        self.spans.push(Span::styled("> ", Theme::dim()));
                    }
                    Tag::CodeBlock(kind) => {
                        self.flush_line();
                        let lang = match kind {
                            CodeBlockKind::Fenced(lang) => {
                                let lang = lang.trim();
                                (!lang.is_empty()).then(|| lang.to_string())
                            }
                            CodeBlockKind::Indented => None,
                        };
                        self.code_block = Some(CodeBlockState {
                            lang,
                            text: String::new(),
                        });
                    }
                    Tag::List(start) => {
                        self.flush_line();
                        self.list_stack.push(ListState {
                            next: start.unwrap_or(1) as usize,
                            ordered: start.is_some(),
                        });
                    }
                    Tag::Item => self.push_list_marker(),
                    Tag::Table(alignments) => {
                        self.flush_line();
                        self.table = Some(TableState {
                            alignments,
                            rows: Vec::new(),
                            current_row: Vec::new(),
                            current_cell: Vec::new(),
                            header_rows: 0,
                        });
                    }
                    Tag::TableHead => {
                        self.in_table_head = true;
                    }
                    Tag::TableRow => {
                        if let Some(table) = self.table.as_mut() {
                            table.current_row.clear();
                        } else {
                            self.flush_line();
                        }
                    }
                    Tag::TableCell => {
                        if let Some(table) = self.table.as_mut() {
                            table.current_cell.clear();
                        }
                        self.in_table_cell = true;
                    }
                    Tag::Strikethrough => {
                        self.style_stack.push(Box::new(|s| {
                            Span::styled(
                                s.to_string(),
                                Theme::dim().add_modifier(Modifier::CROSSED_OUT),
                            )
                        }));
                    }
                    _ => {}
                },
                Event::End(tag) => match tag {
                    TagEnd::Paragraph => self.flush_line(),
                    TagEnd::Heading(_) => {
                        self.flush_line();
                        self.style_stack.pop();
                    }
                    TagEnd::CodeBlock => {
                        if let Some(code) = self.code_block.take() {
                            self.push_code_block(code);
                        }
                    }
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                        self.style_stack.pop();
                    }
                    TagEnd::BlockQuote(_) => self.flush_line(),
                    TagEnd::Item => self.flush_line(),
                    TagEnd::List(_) => {
                        self.flush_line();
                        self.list_stack.pop();
                    }
                    TagEnd::TableHead => {
                        self.in_table_head = false;
                        if let Some(table) = self.table.as_mut() {
                            if !table.current_row.is_empty() {
                                table.rows.push(std::mem::take(&mut table.current_row));
                            }
                            table.header_rows = table.rows.len();
                        }
                    }
                    TagEnd::TableRow => {
                        if let Some(table) = self.table.as_mut() {
                            if !table.current_row.is_empty() {
                                table.rows.push(std::mem::take(&mut table.current_row));
                            }
                        } else {
                            self.flush_line();
                        }
                    }
                    TagEnd::TableCell => {
                        if let Some(table) = self.table.as_mut() {
                            table
                                .current_row
                                .push(trim_cell(std::mem::take(&mut table.current_cell)));
                        }
                        self.in_table_cell = false;
                    }
                    TagEnd::Table => {
                        if let Some(table) = self.table.take() {
                            self.push_table(table);
                        } else {
                            self.flush_line();
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        self.flush_line();
        std::mem::take(&mut self.out)
    }
}

fn code_line(content: TableCell, width: usize) -> Line<'static> {
    let content_width = cell_width(&content);
    let mut spans = vec![Span::styled("|", Theme::dim()), Span::raw(" ")];
    spans.extend(content);
    if width > content_width {
        spans.push(Span::raw(" ".repeat(width - content_width)));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled("|", Theme::dim()));
    Line::from(spans)
}

fn highlight_code_lines(lines: &[&str], lang: Option<&str>) -> Vec<TableCell> {
    let Some(lang) = lang.filter(|lang| !lang.trim().is_empty()) else {
        return plain_code_lines(lines);
    };

    let ps = syntax_set();
    let Some(syntax) = find_syntax_cached(lang) else {
        return plain_code_lines(lines);
    };
    let Some(theme) = theme_set().themes.get("InspiredGitHub") else {
        return plain_code_lines(lines);
    };
    let mut highlighter = HighlightLines::new(syntax, theme);

    lines
        .iter()
        .map(|line| match highlighter.highlight_line(line, ps) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| Span::styled(text.to_string(), syntect_style(style)))
                .collect(),
            Err(_) => vec![plain_code_span(line)],
        })
        .collect()
}

/// Highlight a single line of code in the given language.
/// Returns styled spans with syntax coloring.
/// Falls back to a plain span if the language is unknown.
pub(crate) fn highlight_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let ps = syntax_set();
    let Some(syntax) = find_syntax_cached(lang) else {
        return vec![Span::raw(line.to_string())];
    };
    let Some(theme) = theme_set().themes.get("InspiredGitHub") else {
        return vec![Span::raw(line.to_string())];
    };
    let mut highlighter = HighlightLines::new(syntax, theme);
    match highlighter.highlight_line(line, ps) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_string(), syntect_style(style)))
            .collect(),
        Err(_) => vec![Span::raw(line.to_string())],
    }
}

fn plain_code_lines(lines: &[&str]) -> Vec<TableCell> {
    lines
        .iter()
        .map(|line| vec![plain_code_span(line)])
        .collect()
}

fn plain_code_span(text: &str) -> Span<'static> {
    Span::raw(text.to_string())
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static THEME_SET: std::sync::OnceLock<ThemeSet> = std::sync::OnceLock::new();
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

fn find_syntax<'a>(ps: &'a SyntaxSet, lang: &str) -> Option<&'a syntect::parsing::SyntaxReference> {
    let lang = lang.trim().to_ascii_lowercase();
    ps.syntaxes().iter().find(|syntax| {
        if syntax.name.eq_ignore_ascii_case(&lang) {
            return true;
        }
        if syntax
            .file_extensions
            .iter()
            .any(|ext| ext.eq_ignore_ascii_case(&lang))
        {
            return true;
        }
        let scope = syntax.scope.to_string().to_ascii_lowercase();
        scope.rsplit('.').next() == Some(lang.as_str())
    })
}

/// Cached syntax lookup. `find_syntax` does a linear scan over all
/// loaded syntaxes (~hundreds). Cache the result per language so
/// repeated `highlight_line` calls (e.g. per line in a diff block)
/// only pay the lookup cost once.
fn find_syntax_cached(lang: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Option<&'static syntect::parsing::SyntaxReference>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cache = cache.lock().unwrap();
        if let Some(entry) = cache.get(lang) {
            return *entry;
        }
    }
    let ps = syntax_set();
    let result = ps.find_syntax_by_token(lang).or_else(|| find_syntax(ps, lang));
    {
        let mut cache = cache.lock().unwrap();
        cache.insert(lang.to_string(), result);
    }
    result
}

fn syntect_style(style: SyntectStyle) -> Style {
    let mut out = Style::default()
        .fg(Color::Rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn table_border(left: char, sep: char, right: char, widths: &[usize]) -> String {
    let mut out = String::new();
    out.push(left);
    for (idx, width) in widths.iter().enumerate() {
        if idx > 0 {
            out.push(sep);
        }
        out.push_str(&"-".repeat(width + 2));
    }
    out.push(right);
    out
}

fn label_border(label: &str, width: usize) -> String {
    let label_width = UnicodeWidthStr::width(label);
    let total_inner = width + 2;
    let left = 3.min(total_inner);
    let used = left + label_width + 2;
    if used >= total_inner {
        return table_border('+', '+', '+', &[width]);
    }
    format!(
        "+{} {} {}+",
        "-".repeat(left),
        label,
        "-".repeat(total_inner - used)
    )
}

fn fit_table_width(widths: &mut [usize], max_width: usize) {
    if widths.is_empty() || max_width == usize::MAX / 2 {
        return;
    }
    let fixed = widths.len() * 3 + 1;
    let target = max_width.saturating_sub(fixed).max(widths.len());
    while widths.iter().sum::<usize>() > target {
        if let Some((idx, width)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > 1)
            .max_by_key(|(_, width)| **width)
        {
            let next = width.saturating_sub(1).max(1);
            widths[idx] = next;
        } else {
            break;
        }
    }
}

fn table_row_lines(
    row: &TableRow,
    widths: &[usize],
    alignments: &[Alignment],
) -> Vec<Line<'static>> {
    let cells: Vec<Vec<TableCell>> = widths
        .iter()
        .enumerate()
        .map(|(idx, width)| wrap_cell(row.get(idx).map(Vec::as_slice).unwrap_or(&[]), *width))
        .collect();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|line_idx| {
            let mut spans = vec![Span::styled("|", Theme::dim())];
            for (idx, width) in widths.iter().enumerate() {
                let cell = cells[idx].get(line_idx).map(Vec::as_slice).unwrap_or(&[]);
                let cell_width = cell_width(cell);
                let padding = width.saturating_sub(cell_width);
                let alignment = alignments.get(idx).copied().unwrap_or(Alignment::Left);
                let (left_pad, right_pad) = match alignment {
                    Alignment::Right => (padding, 0),
                    Alignment::Center => (padding / 2, padding - padding / 2),
                    _ => (0, padding),
                };
                spans.push(Span::raw(" "));
                if left_pad > 0 {
                    spans.push(Span::raw(" ".repeat(left_pad)));
                }
                spans.extend(cell.iter().cloned());
                if right_pad > 0 {
                    spans.push(Span::raw(" ".repeat(right_pad)));
                }
                spans.push(Span::raw(" "));
                spans.push(Span::styled("|", Theme::dim()));
            }
            Line::from(spans)
        })
        .collect()
}

fn wrap_cell(cell: &[Span<'static>], width: usize) -> Vec<TableCell> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    for span in cell {
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width > 0 && current_width + ch_width > width {
                lines.push(trim_cell(std::mem::take(&mut current)));
                current_width = 0;
            }
            current.push(Span::styled(ch.to_string(), span.style));
            current_width += ch_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(trim_cell(current));
    }
    lines
}

fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn trim_cell(cell: TableCell) -> TableCell {
    let mut spans: TableCell = cell
        .into_iter()
        .filter_map(|span| {
            if span.content.is_empty() {
                None
            } else {
                Some(span)
            }
        })
        .collect();
    if let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start().to_string();
        first.content = trimmed.into();
    }
    if let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        last.content = trimmed.into();
    }
    spans.retain(|span| !span.content.is_empty());
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join_lines(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn renders_plain_text() {
        let lines = render("hello world");
        assert!(!lines.is_empty(), "got empty output");
        let text = join_lines(&lines);
        assert!(text.contains("hello world"), "got: {text:?}");
    }

    #[test]
    fn renders_bold() {
        let lines = render("**bold** text");
        assert!(!lines.is_empty());
        let text = join_lines(&lines);
        assert!(text.contains("bold"), "bold not found: {text}");
    }

    #[test]
    fn renders_list_markers() {
        let lines = render("- one\n- two\n1. first\n- [x] done\n- [ ] todo");
        let text = join_lines(&lines);
        assert!(text.contains("• one"), "unordered marker missing: {text}");
        assert!(text.contains("• two"), "unordered marker missing: {text}");
        assert!(text.contains("1. first"), "ordered marker missing: {text}");
        assert!(text.contains("[x] done"), "task marker missing: {text}");
        assert!(text.contains("[ ] todo"), "task marker missing: {text}");
    }

    #[test]
    fn renders_code_block() {
        let lines = render("```rust\nfn main() {\n    println!(\"hi\");\n}\n```");
        let text = join_lines(&lines);
        assert!(text.contains("rust"), "language missing:\n{text}");
        assert!(text.contains("fn main()"), "code missing:\n{text}");
        assert!(text.contains("println!"), "code missing:\n{text}");
        assert!(
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|span| span.style.fg.is_some()),
            "syntax highlight style missing"
        );
        assert!(text.contains("+"), "border missing:\n{text}");
        assert!(text.contains("-"), "border missing:\n{text}");
    }

    #[test]
    fn unknown_code_language_renders_plain_text() {
        let lines = render(
            "```code
let x = 1;
```",
        );
        let text = join_lines(&lines);
        assert!(
            text.contains("code"),
            "language label missing:
{text}"
        );
        assert!(
            text.contains("let x = 1;"),
            "code content missing:
{text}"
        );

        let code_line = lines
            .iter()
            .find(|line| {
                let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                joined.contains("let x = 1;")
            })
            .expect("code content line missing");
        for span in &code_line.spans {
            if span.content.trim().is_empty() || span.content.contains('|') {
                continue;
            }
            assert!(
                span.style.fg.is_none() && !span.style.add_modifier.contains(Modifier::REVERSED),
                "unknown language code should be plain text"
            );
        }
    }

    #[test]
    fn renders_simple_table() {
        let md = "| 列 1 | 列 2 | 列 3 |\n|---|---|---|\n| 内 容 1 | 内 容 2 | 内 容 3 |";
        let lines = render(md);
        assert!(!lines.is_empty(), "table must produce lines");
        let text = join_lines(&lines);
        assert!(text.contains("列 1"), "header '列 1' not found:\n{text}");
        assert!(text.contains("内 容"), "cell not found:\n{text}");
        assert!(text.contains("+"), "border not rendered:\n{text}");
        assert!(text.contains("-"), "border not rendered:\n{text}");
        assert!(!text.contains("||"), "raw pipes leaked:\n{text}");
    }

    #[test]
    fn renders_inline_markdown_inside_table_cells() {
        let md = "| 样式 | 效果 |\n|---|---|\n| 加粗 | **文字** |\n| 删除线 | ~~旧需求~~ |\n| 代码 | `重点任务` |";
        let lines = render(md);
        let text = join_lines(&lines);
        assert!(text.contains("文字"), "bold text missing:\n{text}");
        assert!(
            text.contains("旧需求"),
            "strikethrough text missing:\n{text}"
        );
        assert!(text.contains("重点任务"), "code text missing:\n{text}");
        assert!(!text.contains("**文字**"), "bold marker leaked:\n{text}");
        assert!(
            !text.contains("~~旧需求~~"),
            "strikethrough marker leaked:\n{text}"
        );
        assert!(!text.contains("`重点任务`"), "code marker leaked:\n{text}");
    }

    #[test]
    fn renders_real_llm_table() {
        // Exact content from the user's screenshot — LLM output with wide
        // cells and a leading paragraph on the preceding line.
        let md = "当 然 ！ 这 是 一 个 简 单 的  Markdown 表 格 样 例 ：\n\
                  | 姓 名    | 年 龄  | 职 业      |\n\
                  |--------|------|----------|\n\
                  | 张 三    | 28   | 工 程 师    |";
        let lines = render(md);
        assert!(!lines.is_empty());
        let text = join_lines(&lines);
        assert!(text.contains("姓 名"), "header '姓 名' not found:\n{text}");
        assert!(text.contains("张 三"), "cell '张 三' not found:\n{text}");
        // No raw pipes should leak.
        assert!(
            !text.contains("||"),
            "table not rendered (raw pipes):\n{text}"
        );
    }

    #[test]
    fn renders_table_with_crlf() {
        // LLM responses sometimes use \r\n line endings.  pulldown-cmark
        // expects \n, so we must normalize or the table is never detected.
        let md = "| 姓 名 | 年 龄 |\r\n|---|---|\r\n| 张 三 | 25   |";
        let lines = render(md);
        assert!(!lines.is_empty());
        let text = join_lines(&lines);
        assert!(text.contains("姓 名"), "header not found:\n{text}");
        assert!(text.contains("张 三"), "data not found:\n{text}");
        assert!(
            !text.contains("||"),
            "raw pipes leaked (CRLF issue):\n{text}"
        );
    }

    #[test]
    fn renders_exact_screenshot_multiline_table() {
        // The LLM output has a one-line concatenated version (raw pipes,
        // should stay as raw text), then a proper multi-line table under
        // "效果如下：".  The multi-line table MUST be rendered as a table.
        let md = "当 然 ， 这 是 一 个 简 单 的  Markdown 表 格 示 例 ：\n\
                  效 果 如 下 ：\n\
                  | 姓 名    | 年 龄  | 职 业      |\n\
                  |--------|------|----------|\n\
                  | 张 三    | 25   | 程 序 员    |\n\
                  | 李 四    | 30   | 设 计 师    |\n\
                  | 王 五    | 28   | 产 品 经 理  |";
        let lines = render(md);
        let text = join_lines(&lines);
        // The properly formatted multiline table must not have raw pipes.
        assert!(
            !text.contains("||"),
            "multiline table still shows raw pipes:\n{text}"
        );
        assert!(text.contains("姓 名"), "table header missing:\n{text}");
        assert!(text.contains("张 三"), "table data missing:\n{text}");
    }

    #[test]
    fn renders_exact_screenshot_concatenated_table() {
        // The one-liner (header||separator||data) is NOT valid Markdown
        // and should keep its raw pipes.  This test just verifies it
        // doesn't cause a panic.
        let md = "| 姓 名    | 年 龄  | 职 业      ||--------|------|----------|| 张 三    | 25   | 程 序 员    |";
        let lines = render(md);
        assert!(!lines.is_empty(), "concatenated table must produce output");
    }
}
