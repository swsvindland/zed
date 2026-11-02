use database::{ConnectionProfile, sql::ParameterizedStatement};
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    WeakEntity, Window,
};
use ui::{Checkbox, ToggleState, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

use crate::results_view::QueryResultsView;

/// Shows the exact SQL a push will run and asks for confirmation; production connections need
/// an extra explicit acknowledgement.
pub struct PushReviewModal {
    profile: ConnectionProfile,
    preview: SharedString,
    statements: Vec<ParameterizedStatement>,
    results: WeakEntity<QueryResultsView>,
    production_acknowledged: bool,
    focus_handle: FocusHandle,
}

impl PushReviewModal {
    pub fn new(
        profile: ConnectionProfile,
        preview: String,
        statements: Vec<ParameterizedStatement>,
        results: WeakEntity<QueryResultsView>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            profile,
            preview: preview.into(),
            statements,
            results,
            production_acknowledged: false,
            focus_handle: cx.focus_handle(),
        }
    }

    fn can_push(&self) -> bool {
        !self.profile.is_production() || self.production_acknowledged
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_push() {
            return;
        }
        let statements = std::mem::take(&mut self.statements);
        self.results
            .update(cx, |results, cx| results.apply(statements, window, cx))
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for PushReviewModal {}

impl Focusable for PushReviewModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for PushReviewModal {}

impl Render for PushReviewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_production = self.profile.is_production();
        let count = self.statements.len();
        let can_push = self.can_push();
        v_flex()
            .id("database-push-review")
            .key_context("DatabasePushReview")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.push(window, cx)))
            .w(rems(44.))
            .elevation_3(cx)
            .p_4()
            .gap_3()
            .child(
                Headline::new(format!(
                    "Push {count} {} to {}",
                    if count == 1 { "change" } else { "changes" },
                    self.profile.name
                ))
                .size(HeadlineSize::Small),
            )
            .when(is_production, |this| {
                this.child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .rounded_md()
                        .bg(cx.theme().status().error_background)
                        .border_1()
                        .border_color(cx.theme().status().error_border)
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::Warning).color(Color::Error))
                                .child(
                                    Label::new(format!(
                                        "This is a PRODUCTION connection ({}).",
                                        self.profile.environment
                                    ))
                                    .color(Color::Error),
                                ),
                        )
                        .child(
                            Checkbox::new(
                                "database-acknowledge-production",
                                ToggleState::from(self.production_acknowledged),
                            )
                            .label("I understand these statements change production data")
                            .on_click(cx.listener(
                                |this, state: &ToggleState, _, cx| {
                                    this.production_acknowledged = *state == ToggleState::Selected;
                                    cx.notify();
                                },
                            )),
                        ),
                )
            })
            .child(
                Label::new(
                    "Runs in one transaction with bound parameters. Updates and deletes must each \
                     affect exactly one row, or everything is rolled back.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .id("database-push-sql")
                    .max_h(rems(24.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .font_buffer(cx)
                    .text_buffer(cx)
                    .child(self.preview.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("database-push-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new(
                            "database-push-confirm",
                            if is_production {
                                "Push to Production"
                            } else {
                                "Push"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .when(is_production, |button| button.color(Color::Error))
                        .disabled(!can_push)
                        .on_click(cx.listener(|this, _, window, cx| this.push(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
impl PushReviewModal {
    pub(crate) fn preview_for_test(&self) -> String {
        self.preview.to_string()
    }

    pub(crate) fn can_push_for_test(&self) -> bool {
        self.can_push()
    }

    pub(crate) fn push_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push(window, cx);
    }
}
