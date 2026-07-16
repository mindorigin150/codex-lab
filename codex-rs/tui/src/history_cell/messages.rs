//! User, assistant, reasoning, and streaming message history cells.

use super::*;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct UserHistoryCell {
    pub message: String,
    pub text_elements: Vec<TextElement>,
    #[allow(dead_code)]
    pub local_image_paths: Vec<PathBuf>,
    pub remote_image_urls: Vec<String>,
}

/// Remove CSI sequences and control characters, preserving tabs and newlines.
pub(crate) fn sanitize_user_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.next_if_eq(&'[').is_some() {
            let _ = chars.find(|ch| ('@'..='~').contains(ch));
        } else if matches!(ch, '\n' | '\t') || !ch.is_control() {
            sanitized.push(ch);
        }
    }
    sanitized
}

/// Build logical lines for a user message with styled text elements.
///
/// This preserves explicit newlines while interleaving element spans and skips
/// malformed byte ranges instead of panicking during history rendering.
fn build_user_message_lines_with_elements(
    message: &str,
    elements: &[TextElement],
    style: Style,
    element_style: Style,
) -> Vec<Line<'static>> {
    let mut elements = elements.to_vec();
    elements.sort_by_key(|e| e.byte_range.start);
    let mut offset = 0usize;
    let mut raw_lines: Vec<Line<'static>> = Vec::new();
    for line_text in message.split('\n') {
        let line_start = offset;
        let line_end = line_start + line_text.len();
        let mut spans: Vec<Span<'static>> = Vec::new();
        // Track how much of the line we've emitted to interleave plain and styled spans.
        let mut cursor = line_start;
        for elem in &elements {
            let start = elem.byte_range.start.max(line_start);
            let end = elem.byte_range.end.min(line_end);
            if start >= end {
                continue;
            }
            let rel_start = start - line_start;
            let rel_end = end - line_start;
            // Guard against malformed UTF-8 byte ranges from upstream data; skip
            // invalid elements rather than panicking while rendering history.
            if !line_text.is_char_boundary(rel_start) || !line_text.is_char_boundary(rel_end) {
                continue;
            }
            let rel_cursor = cursor - line_start;
            if cursor < start
                && line_text.is_char_boundary(rel_cursor)
                && let Some(segment) = line_text.get(rel_cursor..rel_start)
            {
                spans.push(Span::from(segment.to_string()));
            }
            if let Some(segment) = line_text.get(rel_start..rel_end) {
                spans.push(Span::styled(segment.to_string(), element_style));
                cursor = end;
            }
        }
        let rel_cursor = cursor - line_start;
        if cursor < line_end
            && line_text.is_char_boundary(rel_cursor)
            && let Some(segment) = line_text.get(rel_cursor..)
        {
            spans.push(Span::from(segment.to_string()));
        }
        let line = if spans.is_empty() {
            Line::from(line_text.to_string()).style(style)
        } else {
            Line::from(spans).style(style)
        };
        raw_lines.push(line);
        // Split on '\n' so any '\r' stays in the line; advancing by 1 accounts
        // for the separator byte.
        offset = line_end + 1;
    }

    raw_lines
}

fn remote_image_display_line(style: Style, index: usize) -> Line<'static> {
    Line::from(local_image_label_text(index)).style(style)
}

fn trim_trailing_blank_lines(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    while lines
        .last()
        .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
    {
        lines.pop();
    }
    lines
}

impl HistoryCell for UserHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let message = sanitize_user_text(&self.message);
        let text_elements = if message == self.message {
            self.text_elements.as_slice()
        } else {
            &[]
        };
        let wrap_width = width
            .saturating_sub(
                LIVE_PREFIX_COLS + 1, /* keep a one-column right margin for wrapping */
            )
            .max(1);

        let style = user_message_style();
        let element_style = style.fg(Color::Cyan);

        let wrapped_remote_images = if self.remote_image_urls.is_empty() {
            None
        } else {
            Some(adaptive_wrap_lines(
                self.remote_image_urls
                    .iter()
                    .enumerate()
                    .map(|(idx, _url)| {
                        remote_image_display_line(element_style, idx.saturating_add(1))
                    }),
                RtOptions::new(usize::from(wrap_width))
                    .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
            ))
        };

        let wrapped_message = if message.is_empty() && text_elements.is_empty() {
            None
        } else if text_elements.is_empty() {
            let message_without_trailing_newlines = message.trim_end_matches(['\r', '\n']);
            let wrapped = adaptive_wrap_lines(
                message_without_trailing_newlines
                    .split('\n')
                    .map(|line| Line::from(line).style(style)),
                // Wrap algorithm matches textarea.rs.
                RtOptions::new(usize::from(wrap_width))
                    .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
            );
            let wrapped = trim_trailing_blank_lines(wrapped);
            (!wrapped.is_empty()).then_some(wrapped)
        } else {
            let raw_lines = build_user_message_lines_with_elements(
                &message,
                text_elements,
                style,
                element_style,
            );
            let wrapped = adaptive_wrap_lines(
                raw_lines,
                RtOptions::new(usize::from(wrap_width))
                    .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
            );
            let wrapped = trim_trailing_blank_lines(wrapped);
            (!wrapped.is_empty()).then_some(wrapped)
        };

        if wrapped_remote_images.is_none() && wrapped_message.is_none() {
            return Vec::new();
        }

        let mut lines: Vec<Line<'static>> = vec![Line::from("").style(style)];

        if let Some(wrapped_remote_images) = wrapped_remote_images {
            lines.extend(prefix_lines(
                wrapped_remote_images,
                "  ".into(),
                "  ".into(),
            ));
            if wrapped_message.is_some() {
                lines.push(Line::from("").style(style));
            }
        }

        if let Some(wrapped_message) = wrapped_message {
            lines.extend(prefix_lines(
                wrapped_message,
                "› ".bold().dim(),
                "  ".into(),
            ));
        }

        lines.push(Line::from("").style(style));
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let message = sanitize_user_text(&self.message);
        let mut lines = raw_lines_from_source(message.trim_end_matches(['\r', '\n']));
        if !self.remote_image_urls.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.extend(
                self.remote_image_urls
                    .iter()
                    .enumerate()
                    .map(|(idx, _url)| Line::from(local_image_label_text(idx.saturating_add(1)))),
            );
        }
        lines
    }
}

#[derive(Debug)]
pub(crate) struct ReasoningSummaryCell {
    _header: String,
    content: String,
    /// Session cwd used to render local file links inside the reasoning body.
    cwd: PathBuf,
    transcript_only: bool,
}

impl ReasoningSummaryCell {
    /// Create a reasoning summary cell that will render local file links relative to the session
    /// cwd active when the summary was recorded.
    pub(crate) fn new(header: String, content: String, cwd: &Path, transcript_only: bool) -> Self {
        Self {
            _header: header,
            content,
            cwd: cwd.to_path_buf(),
            transcript_only,
        }
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_markdown(
            &self.content,
            crate::width::usable_content_width_u16(width, /*reserved_cols*/ 2),
            Some(self.cwd.as_path()),
            &mut lines,
        );
        let summary_style = Style::default().dim().italic();
        let summary_lines = lines
            .into_iter()
            .map(|mut line| {
                line.spans = line
                    .spans
                    .into_iter()
                    .map(|span| span.patch_style(summary_style))
                    .collect();
                line
            })
            .collect::<Vec<_>>();

        adaptive_wrap_lines(
            &summary_lines,
            RtOptions::new(width as usize)
                .initial_indent("• ".dim().into())
                .subsequent_indent("  ".into()),
        )
    }
}

impl HistoryCell for ReasoningSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.transcript_only {
            Vec::new()
        } else {
            self.lines(width)
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        if self.transcript_only {
            Vec::new()
        } else {
            raw_lines_from_source(self.content.trim())
        }
    }
}

#[derive(Debug)]
pub(crate) struct AgentMessageCell {
    lines: Vec<HyperlinkLine>,
    is_first_line: bool,
}

impl AgentMessageCell {
    #[cfg(test)]
    pub(crate) fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines: plain_hyperlink_lines(lines),
            is_first_line,
        }
    }

    pub(crate) fn new_hyperlink_lines(lines: Vec<HyperlinkLine>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl HistoryCell for AgentMessageCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.display_hyperlink_lines(width))
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        let mut wrapped = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            let initial_indent = if index == 0 && self.is_first_line {
                "• ".dim().into()
            } else {
                "  ".into()
            };
            let mut subsequent_indent = Line::from("  ");
            subsequent_indent
                .spans
                .extend(crate::insert_history::leading_whitespace_prefix(&line.line).spans);
            wrapped.extend(crate::terminal_hyperlinks::adaptive_wrap_hyperlink_lines(
                std::slice::from_ref(line),
                RtOptions::new(width as usize)
                    .initial_indent(initial_indent)
                    .subsequent_indent(subsequent_indent),
            ));
        }
        wrapped
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(visible_lines(self.lines.clone()))
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}

/// A consolidated agent message cell that stores raw markdown source and re-renders from it.
///
/// After a stream finalizes, the `ConsolidateAgentMessage` handler in `App`
/// replaces the contiguous run of `AgentMessageCell`s with a single
/// `AgentMarkdownCell`. On terminal resize, `display_lines(width)` re-renders
/// from source via `append_markdown_agent`, producing correctly-sized tables
/// with box-drawing borders.
///
/// The cell snapshots `cwd` at construction so local file-link display remains aligned with the
/// session that produced the message. Reusing the current process cwd during reflow would make old
/// transcript content change meaning after a later `/cd` or resumed session.
#[derive(Debug)]
pub(crate) struct AgentMarkdownCell {
    cell_id: u64,
    markdown_source: String,
    cwd: PathBuf,
    formulas: Arc<crate::formula_runtime::FormulaMessageState>,
}

impl AgentMarkdownCell {
    /// Create a finalized source-backed assistant message cell.
    ///
    /// `markdown_source` must be the raw source accumulated by the stream controller, not already
    /// wrapped terminal lines. Passing rendered lines here would make future resize reflow preserve
    /// stale wrapping instead of repairing it.
    pub(crate) fn new(markdown_source: String, cwd: &Path) -> Self {
        static NEXT_CELL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let formulas = crate::formula_runtime::FormulaMessageState::new(&markdown_source);
        Self {
            cell_id: NEXT_CELL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            markdown_source,
            cwd: cwd.to_path_buf(),
            formulas,
        }
    }

    pub(crate) fn cell_id(&self) -> u64 {
        self.cell_id
    }

    pub(crate) fn has_formulas(&self) -> bool {
        self.formulas.has_formulas()
    }

    pub(crate) fn prepare_formulas(
        &self,
        key: crate::formula_runtime::FormulaRenderKey,
        app_event_tx: crate::app_event_sender::AppEventSender,
    ) -> bool {
        self.formulas.prepare(key, self.cell_id, app_event_tx)
    }

    pub(crate) fn deactivate_formulas(&self) {
        self.formulas.deactivate();
    }

    pub(crate) fn formulas_ready(&self, key: crate::formula_runtime::FormulaRenderKey) -> bool {
        self.formulas.is_ready(key)
    }

    pub(crate) fn take_formula_errors(&self, width: u16) -> Vec<String> {
        self.formulas.take_errors(width)
    }

    fn rich_markdown_lines(&self, width: u16) -> Vec<RichHistoryLine> {
        let Some(wrap_width) =
            crate::width::usable_content_width_u16(width, /*reserved_cols*/ 2)
        else {
            return vec![RichHistoryLine::plain(HyperlinkLine::new(Line::default()))];
        };
        let Some(assets) = self.formulas.ready_assets(width) else {
            return crate::markdown::render_markdown_agent_with_links_and_cwd(
                &self.markdown_source,
                Some(wrap_width),
                Some(self.cwd.as_path()),
            )
            .into_iter()
            .map(RichHistoryLine::plain)
            .collect();
        };
        let masked = masked_formula_markdown(&self.markdown_source, &assets);
        let rendered = crate::markdown::render_markdown_agent_with_links_and_cwd(
            &masked,
            Some(wrap_width),
            Some(self.cwd.as_path()),
        );
        materialize_formula_lines(rendered, &assets, wrap_width as u16)
    }
}

impl HistoryCell for AgentMarkdownCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_rich_lines(width)
            .into_iter()
            .map(|line| line.text.line)
            .collect()
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_rich_lines(width)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    fn display_rich_lines(&self, width: u16) -> Vec<RichHistoryLine> {
        prefix_rich_formula_lines(self.rich_markdown_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn transcript_rich_lines(&self, width: u16) -> Vec<RichHistoryLine> {
        self.display_rich_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.markdown_source)
    }
}

const FORMULA_MARKER_START: u32 = 0xe000;
const ZERO_WIDTH_BREAK: char = '\u{200b}';
const FORMULA_TOKEN_START: char = '\u{2063}';
const FORMULA_TOKEN_END: char = '\u{2064}';

fn formula_marker(index: usize) -> char {
    let Some(marker) = char::from_u32(FORMULA_MARKER_START + index as u32) else {
        unreachable!("formula marker range exhausted");
    };
    marker
}

fn formula_index(marker: char) -> Option<usize> {
    let value = marker as u32;
    (FORMULA_MARKER_START..FORMULA_MARKER_START + 128)
        .contains(&value)
        .then(|| (value - FORMULA_MARKER_START) as usize)
}

fn masked_formula_markdown(
    source: &str,
    assets: &[Result<crate::formula_runtime::FormulaAsset, String>],
) -> String {
    let mut masked = String::with_capacity(source.len());
    let mut copied = 0usize;
    for (index, asset) in assets.iter().enumerate() {
        let Ok(asset) = asset else {
            continue;
        };
        masked.push_str(&source[copied..asset.source.source_range.start]);
        let marker = formula_marker(index);
        let token = marker.to_string().repeat(usize::from(asset.layout.columns));
        if asset.layout.is_block {
            masked.push_str("\n\n");
            masked.push(ZERO_WIDTH_BREAK);
            masked.push(FORMULA_TOKEN_START);
            masked.push_str(&token);
            masked.push(FORMULA_TOKEN_END);
            masked.push(ZERO_WIDTH_BREAK);
            masked.push_str("\n\n");
        } else {
            masked.push(ZERO_WIDTH_BREAK);
            masked.push(FORMULA_TOKEN_START);
            masked.push_str(&token);
            masked.push(FORMULA_TOKEN_END);
            masked.push(ZERO_WIDTH_BREAK);
        }
        copied = asset.source.source_range.end;
    }
    masked.push_str(&source[copied..]);
    masked
}

fn materialize_formula_lines(
    lines: Vec<HyperlinkLine>,
    assets: &[Result<crate::formula_runtime::FormulaAsset, String>],
    width: u16,
) -> Vec<RichHistoryLine> {
    let mut output = Vec::new();
    let mut inside_formula_token = false;
    for mut line in lines {
        let mut column = 0usize;
        let mut placements = Vec::new();
        let mut spans = Vec::with_capacity(line.line.spans.len());
        for span in line.line.spans {
            let mut text = String::with_capacity(span.content.len());
            for ch in span.content.chars() {
                if ch == FORMULA_TOKEN_START {
                    inside_formula_token = true;
                    continue;
                }
                if ch == FORMULA_TOKEN_END {
                    inside_formula_token = false;
                    continue;
                }
                if inside_formula_token
                    && let Some(index) = formula_index(ch)
                    && let Some(Ok(asset)) = assets.get(index)
                {
                    if placements
                        .last()
                        .is_none_or(|placement: &FormulaPlacement| placement.formula_index != index)
                    {
                        placements.push(FormulaPlacement {
                            formula_index: index,
                            column: column as u16,
                            columns: asset.layout.columns,
                            rows: asset.layout.rows,
                            raster: asset.layout.raster.clone(),
                        });
                    }
                    text.push(' ');
                    column += 1;
                } else {
                    text.push(ch);
                    column += ch.to_string().width();
                }
            }
            spans.push(Span::styled(text, span.style));
        }
        line.line.spans = spans;
        for placement in &mut placements {
            if assets[placement.formula_index]
                .as_ref()
                .is_ok_and(|asset| asset.layout.is_block)
            {
                placement.column = placement
                    .column
                    .max(width.saturating_sub(placement.columns) / 2);
            }
        }
        let continuation_rows = placements
            .iter()
            .map(|placement| placement.rows.saturating_sub(1))
            .max()
            .unwrap_or(0);
        output.push(RichHistoryLine {
            text: line,
            formulas: placements,
        });
        output.extend((0..continuation_rows).map(|_| {
            RichHistoryLine::plain(HyperlinkLine::new(Line::from(
                " ".repeat(usize::from(width)),
            )))
        }));
    }
    output
}

fn prefix_rich_formula_lines(lines: Vec<RichHistoryLine>) -> Vec<RichHistoryLine> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let initial = if index == 0 {
                "• ".dim()
            } else {
                "  ".into()
            };
            let Some(prefixed) = prefix_hyperlink_lines(vec![line.text], initial, "  ".into())
                .into_iter()
                .next()
            else {
                unreachable!("prefixing one history line returned no lines");
            };
            line.text = prefixed;
            for placement in &mut line.formulas {
                placement.column += 2;
            }
            line
        })
        .collect()
}

/// Transient active-cell representation of the mutable tail of an agent stream.
///
/// During streaming, lines that have not yet been committed to scrollback because they belong to
/// an in-progress table are displayed via this cell in the `active_cell` slot. It is replaced on
/// every delta and cleared when the stream finalizes.
#[derive(Debug)]
pub(crate) struct StreamingAgentTailCell {
    lines: Vec<HyperlinkLine>,
    is_first_line: bool,
}

impl StreamingAgentTailCell {
    pub(crate) fn new(lines: Vec<HyperlinkLine>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl HistoryCell for StreamingAgentTailCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.display_hyperlink_lines(width))
    }

    fn display_hyperlink_lines(&self, _width: u16) -> Vec<HyperlinkLine> {
        // Tail lines are already rendered at the controller's current stream width.
        // Re-wrapping them here can split table borders and produce malformed in-flight rows.
        let mut lines = prefix_hyperlink_lines(
            self.lines.clone(),
            if self.is_first_line {
                "• ".dim()
            } else {
                "  ".into()
            },
            "  ".into(),
        );
        for line in &mut lines {
            if line
                .line
                .spans
                .iter()
                .all(|span| span.content.chars().all(char::is_whitespace))
            {
                line.line = Line::default().style(line.line.style);
                line.hyperlinks.clear();
            }
        }
        lines
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(/*width*/ u16::MAX))
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}
pub(crate) fn new_user_prompt(
    message: String,
    text_elements: Vec<TextElement>,
    local_image_paths: Vec<PathBuf>,
    remote_image_urls: Vec<String>,
) -> UserHistoryCell {
    UserHistoryCell {
        message,
        text_elements,
        local_image_paths,
        remote_image_urls,
    }
}
/// Create the reasoning history cell emitted at the end of a reasoning block.
///
/// The helper snapshots `cwd` into the returned cell so local file links render the same way they
/// did while the turn was live, even if rendering happens after other app state has advanced. Part
/// boundaries are preserved so standalone empty placeholders can be removed without changing
/// literal HTML comments or bold-only summary content.
pub(crate) fn new_reasoning_summary_block(
    reasoning_parts: Vec<String>,
    cwd: &Path,
) -> Box<dyn HistoryCell> {
    let (header, content) = split_reasoning_summary_parts(&reasoning_parts);
    let transcript_only = header.is_empty();
    Box::new(ReasoningSummaryCell::new(
        header,
        content,
        cwd,
        transcript_only,
    ))
}

/// Split structured reasoning-summary parts into the status header and renderable content.
pub(crate) fn split_reasoning_summary_parts(reasoning_parts: &[String]) -> (String, String) {
    let mut leading_empty_part_header = None;
    let mut content_parts = Vec::with_capacity(reasoning_parts.len());

    for part in reasoning_parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let header_end = part.strip_prefix("**").and_then(|after_open| {
            after_open
                .find("**")
                .and_then(|close| (close > 0).then_some(close + 4))
        });
        let body = header_end.map_or(part, |header_end| &part[header_end..]);
        if body.trim() == "<!-- -->" {
            if content_parts.is_empty()
                && leading_empty_part_header.is_none()
                && let Some(header_end) = header_end
            {
                leading_empty_part_header = Some(part[..header_end].to_string());
            }
            continue;
        }

        content_parts.push(part);
    }

    let content = content_parts.join("\n\n");
    if content.is_empty() {
        return (leading_empty_part_header.unwrap_or_default(), content);
    }

    if let Some(after_open) = content.strip_prefix("**")
        && let Some(close) = after_open.find("**")
    {
        let after_close_idx = 2 + close + 2;
        let after_close = &content[after_close_idx..];
        if after_close.starts_with('\n') || after_close.starts_with('\r') {
            return (
                content[..after_close_idx].to_string(),
                after_close.to_string(),
            );
        }
    }

    (leading_empty_part_header.unwrap_or_default(), content)
}

#[cfg(test)]
mod formula_image_tests {
    use super::*;

    #[test]
    fn finalized_agent_markdown_materializes_formula_placements() {
        let cell = AgentMarkdownCell::new(
            "Inline $x+1$ then block:\n\n$$\\frac{a}{b}$$".to_string(),
            Path::new("/tmp"),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let app_event_tx = crate::app_event_sender::AppEventSender::new(tx);
        let key = crate::formula_runtime::FormulaRenderKey {
            width: 80,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            foreground_rgb: [230, 230, 230],
        };

        assert!(cell.prepare_formulas(key, app_event_tx));
        assert!(matches!(
            rx.blocking_recv(),
            Some(crate::app_event::AppEvent::FormulaRenderReady { cell_id })
                if cell_id == cell.cell_id()
        ));

        let lines = cell.display_rich_lines(80);
        let errors = cell.take_formula_errors(80);
        assert!(errors.is_empty(), "formula errors: {errors:?}");
        let placements = lines
            .iter()
            .flat_map(|line| line.formulas.iter())
            .collect::<Vec<_>>();
        assert_eq!(placements.len(), 2, "rendered lines: {lines:?}");
        assert!(placements.iter().any(|placement| placement.rows > 1));
        assert!(lines.iter().all(|line| {
            line.text
                .line
                .spans
                .iter()
                .flat_map(|span| span.content.chars())
                .all(|ch| {
                    formula_index(ch).is_none()
                        && ch != FORMULA_TOKEN_START
                        && ch != FORMULA_TOKEN_END
                })
        }));
        assert_eq!(
            cell.markdown_source,
            "Inline $x+1$ then block:\n\n$$\\frac{a}{b}$$"
        );
    }

    #[test]
    fn renders_vscode_replayed_formula_source() {
        let cell = AgentMarkdownCell::new(
            "# Fit \\(\\Delta\\)\n\n\\[\n\\Delta =\naction\\_ready\\_ms-worker\\_service\\_ms\n\\]\n\nRuntime: \\(action\\_ready=L_r+R_r+\\Delta\\)".to_string(),
            Path::new("/tmp"),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let key = crate::formula_runtime::FormulaRenderKey {
            width: 80,
            cell_pixel_width: 9,
            cell_pixel_height: 18,
            foreground_rgb: [230, 230, 230],
        };

        assert!(cell.prepare_formulas(key, crate::app_event_sender::AppEventSender::new(tx)));
        assert!(matches!(
            rx.blocking_recv(),
            Some(crate::app_event::AppEvent::FormulaRenderReady { .. })
        ));

        let lines = cell.display_rich_lines(80);
        let errors = cell.take_formula_errors(80);
        assert!(errors.is_empty(), "formula errors: {errors:?}");
        assert_eq!(
            lines.iter().flat_map(|line| line.formulas.iter()).count(),
            3
        );
    }

    #[test]
    fn renders_learning_rate_formulas_in_markdown_table() {
        let cell = AgentMarkdownCell::new(
            "| Parameter | Learning rate |\n|---|---:|\n| VLA backbone / decoder | \\(2\\times10^{-5}\\) |\n| Vision-language connector | \\(1\\times10^{-5}\\) |\n| Action head | \\(1\\times10^{-4}\\) |\n\n\\[\n\\text{connector LR}=10^{-5},\\qquad\n\\text{action-head LR}=10^{-4}\n\\]"
                .to_string(),
            Path::new("/tmp"),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let key = crate::formula_runtime::FormulaRenderKey {
            width: 101,
            cell_pixel_width: 8,
            cell_pixel_height: 18,
            foreground_rgb: [171, 178, 191],
        };

        assert!(cell.prepare_formulas(key, crate::app_event_sender::AppEventSender::new(tx)));
        assert!(matches!(
            rx.blocking_recv(),
            Some(crate::app_event::AppEvent::FormulaRenderReady { .. })
        ));

        let lines = cell.display_rich_lines(101);
        let errors = cell.take_formula_errors(101);
        assert!(errors.is_empty(), "formula errors: {errors:?}");
        let placements = lines
            .iter()
            .flat_map(|line| line.formulas.iter())
            .collect::<Vec<_>>();
        assert_eq!(placements.len(), 4, "rendered lines: {lines:?}");
        assert_eq!(
            placements
                .iter()
                .filter(|placement| placement.rows == 1)
                .count(),
            3
        );
        assert!(placements.iter().any(|placement| placement.rows == 2));
    }

    #[test]
    fn user_private_use_characters_are_not_formula_markers() {
        let source = "Private \u{e07f} then $x$";
        let cell = AgentMarkdownCell::new(source.to_string(), Path::new("/tmp"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let key = crate::formula_runtime::FormulaRenderKey {
            width: 80,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            foreground_rgb: [230, 230, 230],
        };

        assert!(cell.prepare_formulas(key, crate::app_event_sender::AppEventSender::new(tx)));
        assert!(matches!(
            rx.blocking_recv(),
            Some(crate::app_event::AppEvent::FormulaRenderReady { .. })
        ));

        let rendered = cell
            .display_rich_lines(80)
            .into_iter()
            .flat_map(|line| line.text.line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(rendered.contains('\u{e07f}'));
    }
}
