//! Transcript consolidation for finalized streaming agent messages.
//!
//! During streaming, the chat widget emits transient `AgentMessageCell`s so it
//! can animate stable lines into scrollback while keeping the active mutable
//! tail in the bottom pane. Once the answer finishes, the app replaces that
//! trailing run with a single source-backed `AgentMarkdownCell`. This makes the
//! transcript the canonical owner of the raw markdown source used for future
//! resize re-renders.

use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::eyre::Result;

use super::App;
use super::resize_reflow::trailing_run_start;
use crate::app_event::ConsolidationScrollbackReflow;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::pager_overlay::Overlay;
use crate::tui;

impl App {
    pub(super) fn handle_consolidate_agent_message(
        &mut self,
        tui: &mut tui::Tui,
        source: String,
        cwd: PathBuf,
        scrollback_reflow: ConsolidationScrollbackReflow,
        deferred_history_cell: Option<Box<dyn HistoryCell>>,
    ) -> Result<()> {
        // Some finalize paths must preserve a last provisional stream cell long
        // enough for queue ordering, then fold it into the canonical
        // source-backed cell during consolidation.
        if let Some(cell) = deferred_history_cell {
            let cell: Arc<dyn HistoryCell> = cell.into();
            if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                t.insert_cell(cell.clone());
            }
            self.transcript_cells.push(cell);
        }

        // Walk backward to find the contiguous run of streaming AgentMessageCells that
        // belong to the just-finalized stream.
        let end = self.transcript_cells.len();
        tracing::debug!(
            "ConsolidateAgentMessage: transcript_cells.len()={end}, source_len={}",
            source.len()
        );
        let start = trailing_run_start::<history_cell::AgentMessageCell>(&self.transcript_cells);
        if start < end {
            tracing::debug!(
                "ConsolidateAgentMessage: replacing cells [{start}..{end}] with AgentMarkdownCell"
            );
            let consolidated = Arc::new(history_cell::AgentMarkdownCell::new(source, &cwd));
            let formula_reflow_pending = if consolidated.has_formulas() {
                let terminal_width = tui.terminal.size()?.width;
                let width = self.chat_widget.history_wrap_width(terminal_width);
                if let Some(key) = tui.formula_render_key(width) {
                    consolidated.prepare_formulas(key, self.app_event_tx.clone())
                } else {
                    show_formula_capability_warning_once(self, tui);
                    false
                }
            } else {
                false
            };
            let consolidated: Arc<dyn HistoryCell> = consolidated;
            self.transcript_cells
                .splice(start..end, std::iter::once(consolidated.clone()));

            if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                t.consolidate_cells(start..end, consolidated.clone());
                tui.frame_requester().schedule_frame();
            }

            if !formula_reflow_pending {
                self.finish_agent_message_consolidation(tui, scrollback_reflow)?;
            }
        } else {
            tracing::debug!(
                "ConsolidateAgentMessage: no cells to consolidate(start={start}, end={end})",
            );
            self.maybe_finish_stream_reflow(tui)?;
        }

        Ok(())
    }

    fn finish_agent_message_consolidation(
        &mut self,
        tui: &mut tui::Tui,
        scrollback_reflow: ConsolidationScrollbackReflow,
    ) -> Result<()> {
        match scrollback_reflow {
            ConsolidationScrollbackReflow::IfResizeReflowRan => {
                self.maybe_finish_stream_reflow(tui)?;
            }
            ConsolidationScrollbackReflow::Required => {
                self.finish_required_stream_reflow(tui)?;
            }
        }

        Ok(())
    }
}

pub(super) fn show_formula_capability_warning_once(app: &mut App, tui: &mut tui::Tui) {
    static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let warning: Arc<dyn HistoryCell> = Arc::new(history_cell::new_error_event(
        "LaTeX image rendering is unavailable: VS Code Kitty graphics or CSI 16t cell-size probing did not respond. Formula source was preserved."
            .to_string(),
    ));
    if let Some(Overlay::Transcript(transcript)) = &mut app.overlay {
        transcript.insert_cell(warning.clone());
        tui.frame_requester().schedule_frame();
    }
    app.transcript_cells.push(warning);
}
