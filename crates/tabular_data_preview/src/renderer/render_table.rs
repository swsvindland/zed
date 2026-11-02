use crate::types::TableCell;
use gpui::{AnyElement, Entity, Hsla, MouseButton, MouseDownEvent};
use std::ops::Range;
use ui::{ColumnWidthConfig, ResizableColumnsState, Table, UncheckedTableRow, div, prelude::*};

use crate::{
    TableView,
    settings::RowRenderMechanism,
    types::{AnyColumn, DataCellId, DisplayCellId, DisplayRow},
};

impl TableView {
    pub(crate) fn create_table(
        &self,
        current_widths: &Entity<ResizableColumnsState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_count = self.engine.d2d_mapping().visible_row_count();
        let cols = current_widths.read(cx).cols();
        let mut headers = Vec::with_capacity(cols);

        headers.push(self.create_row_identifier_header(cx));

        for i in 0..(cols - 1) {
            let header_text = self
                .engine
                .contents
                .headers
                .get(AnyColumn(i))
                .and_then(|h| h.display_value().cloned())
                .unwrap_or_else(|| format!("Col {}", i + 1).into());

            headers.push(self.create_header_element_with_sort_button(
                header_text,
                cx,
                AnyColumn::from(i),
            ));
        }

        Table::new(cols)
            .interactable(&self.table_interaction_state)
            .width_config(ColumnWidthConfig::Resizable(current_widths.clone()))
            .header(headers)
            .disable_base_style()
            .pin_cols(1)
            .map(|table| {
                let row_identifier_text_color = cx.theme().colors().editor_line_number;
                match self.settings.rendering_with {
                    RowRenderMechanism::VariableList => {
                        table.variable_row_height_list(row_count, self.list_state.clone(), {
                            cx.processor(move |this, display_row: usize, _window, cx| {
                                this.performance_metrics.rendered_indices.push(display_row);
                                // The mapping may be transiently stale while a filter/sort task is in-flight. Return an empty row rather than panicking
                                Self::render_single_table_row(
                                    this,
                                    cols,
                                    DisplayRow(display_row),
                                    row_identifier_text_color,
                                    this.row_height,
                                    cx,
                                )
                                .unwrap_or_default()
                            })
                        })
                    }
                    RowRenderMechanism::UniformList => {
                        table.uniform_list("tabular-data-table", row_count, {
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                this.performance_metrics
                                    .rendered_indices
                                    .extend(range.clone());

                                let row_height = this.row_height;
                                range
                                    .filter_map(|display_index| {
                                        Self::render_single_table_row(
                                            this,
                                            cols,
                                            DisplayRow(display_index),
                                            row_identifier_text_color,
                                            row_height,
                                            cx,
                                        )
                                    })
                                    .collect()
                            })
                        })
                    }
                }
            })
            .into_any_element()
    }

    /// Render a single table row
    ///
    /// Used both by UniformList and VariableRowHeightList
    fn render_single_table_row(
        this: &TableView,
        cols: usize,
        display_row: DisplayRow,
        row_identifier_text_color: Hsla,
        row_height: Pixels,
        cx: &Context<TableView>,
    ) -> Option<UncheckedTableRow<AnyElement>> {
        // Get the actual row index from our sorted indices
        let data_row = this.engine.d2d_mapping().get_data_row(display_row)?;
        let row = this.engine.contents.get_row(data_row)?;

        let mut elements = Vec::with_capacity(cols);
        elements.push(this.create_row_identifier_cell(display_row, data_row, cx)?);

        // Remaining columns: actual table data
        for col in (0..this.engine.contents.number_of_cols).map(AnyColumn) {
            let table_cell = row.expect_get(col);

            let display_cell_id = DisplayCellId::new(display_row, col);
            let data_cell_id = DataCellId::new(data_row, col);
            let decoration = this.cell_decorations.get(&data_cell_id);
            let row_decoration = this.row_decorations.get(&data_row);

            // TODO: Introduce `<null>` cell type
            let cell_content = decoration
                .and_then(|decoration| decoration.text.clone())
                .or_else(|| table_cell.display_value().cloned())
                .unwrap_or_default();
            let is_muted = decoration
                .or(row_decoration)
                .is_some_and(|decoration| decoration.muted);
            let decoration_bg = decoration
                .and_then(|decoration| decoration.background)
                .or_else(|| row_decoration.and_then(|decoration| decoration.background));

            let is_focus_cell = this
                .selection
                .as_ref()
                .is_some_and(|selection| selection.focus == data_cell_id);

            let cell_bg = match (is_focus_cell, decoration_bg) {
                (_, Some(background)) => Some(background),
                (true, None) => Some(cx.theme().colors().element_selected),
                (false, None) => None,
            };
            let cell_editor = this
                .cell_editor
                .as_ref()
                .filter(|(cell, _)| *cell == data_cell_id)
                .map(|(_, editor)| editor.clone());

            let cell = div()
                .size_full()
                .when(
                    !this.settings.multiline_cells_effectively_enabled(),
                    |div| {
                        div.whitespace_nowrap()
                            .text_ellipsis()
                            .h(row_height)
                            .overflow_hidden()
                    },
                )
                .when(is_muted, |div| {
                    div.text_color(cx.theme().colors().text_muted)
                })
                .child(match cell_editor {
                    Some(editor) => div().size_full().px_1().child(editor).into_any_element(),
                    None => TableView::create_selectable_cell(
                        display_cell_id,
                        cell_content,
                        this.settings.vertical_alignment,
                        cx,
                    ),
                });

            elements.push(
                div()
                    .size_full()
                    .when_some(cell_bg, |div, color| div.bg(color))
                    .when(is_focus_cell && decoration_bg.is_some(), |div| {
                        div.border_1()
                            .border_color(cx.theme().colors().border_focused)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.select_cell_with_mouse(
                                data_cell_id,
                                event.click_count,
                                window,
                                cx,
                            );
                        }),
                    )
                    .when(this.settings.show_debug_info, |parent| {
                        parent.child(div().text_color(row_identifier_text_color).child(
                            match table_cell {
                                TableCell::Real { position: pos, .. } => {
                                    let slv = pos.start.timestamp().value;
                                    let so = pos.start.offset;
                                    let elv = pos.end.timestamp().value;
                                    let eo = pos.end.offset;
                                    format!("Pos {so}(L{slv})-{eo}(L{elv})")
                                }
                                TableCell::Virtual => "Virtual cell".into(),
                                TableCell::Generated(_) => "Generated cell".into(),
                            },
                        ))
                    })
                    .child(cell)
                    .into_any_element(),
            );
        }

        Some(elements)
    }
}
