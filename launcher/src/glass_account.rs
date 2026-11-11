//! Settings' Account card in the glass look, as the mockup's: who plays now (a big head, the
//! name, how), and the Ely.by card beside it with its Sign in with Ely.by button and form.
//!
//! Not in the mockup, kept in its patterns: the Ely.by accounts already signed in (in the
//! Ely.by card, with Play as, Refresh and Sign out), the sign-in's progress and problems.

use super::*;
use crate::glass_ui::{self as ui, white, Tone};
use horizon_theme::glass::ink;

impl Launcher {
    pub(super) fn glass_account(&mut self, cx: &mut Context<Self>) -> Div {
        let wide = self.window_width >= crate::glass_home::WIDE;
        let left = self.glass_who_plays();
        let right = self.glass_ely_card(cx);
        if wide {
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(left.flex_1().min_w(px(280.)).p(px(20.)))
                .child(right.flex_none().w(px(360.)))
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(16.))
                .child(left.w_full().min_w_0().p(px(20.)))
                .child(right.w_full())
        }
    }

    /// The player now.
    fn glass_who_plays(&self) -> Div {
        let t = self.theme;
        let (uuid, name, status) = match self.accounts.selected() {
            Some(a) => (
                Some(a.uuid.clone()),
                a.name.clone(),
                (Tone::Live, tr("Ely.by account · plays online")),
            ),
            None => (
                None,
                tr("Sign in with Ely.by").to_string(),
                (Tone::Off, tr(account::NEEDS_ACCOUNT)),
            ),
        };
        let head = div()
            .flex_none()
            .size(px(80.))
            .rounded(px(20.))
            .overflow_hidden()
            .child(
                self.profile_heads
                    .draw(uuid.as_deref(), 80., self.scale, &t),
            );
        let header = div().flex().items_center().gap(px(20.)).child(head).child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .gap(px(10.))
                .child(
                    div()
                        .font_family(horizon_theme::glass::DISPLAY_FONT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(32.))
                        .line_height(px(32.))
                        .text_color(white(ink::FULL))
                        .truncate()
                        .child(name),
                )
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(ui::dot(status.0).mt(px(7.)))
                        .child(
                            ui::text(14., FontWeight::NORMAL, 0.65)
                                .flex_1()
                                .min_w_0()
                                .line_height(px(21.))
                                .child(status.1),
                        ),
                ),
        );

        div().flex().flex_col().child(header)
    }

    /// The Ely.by card: the accounts signed in here, then adding one (the mockup's card).
    fn glass_ely_card(&mut self, cx: &mut Context<Self>) -> Div {
        let has_accounts = !self.accounts.accounts.is_empty();
        let (rows, _) = if has_accounts {
            ui::flat_rows(|| self.signed_in_rows(cx))
        } else {
            (div(), 0)
        };
        let (title, detail) = self.sign_in_parts(cx);
        let heading = if has_accounts {
            title
        } else {
            tr("Play online with your Ely.by account").to_string()
        };
        div()
            .flex()
            .flex_col()
            .rounded(px(16.))
            .bg(white(0.045))
            .p(px(24.))
            .when(has_accounts, |d| {
                d.child(rows.mx(px(-16.)).mt(px(-16.)).mb(px(16.)))
            })
            .child(ui::icon("user", 36., white(ink::FULL)))
            .child(
                div()
                    .mt(px(20.))
                    .font_family(horizon_theme::glass::DISPLAY_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(22.))
                    .line_height(px(27.5))
                    .text_color(white(ink::FULL))
                    .child(heading),
            )
            .child(
                ui::text(14., FontWeight::NORMAL, 0.65)
                    .mt(px(8.))
                    .line_height(px(21.7))
                    .child(detail),
            )
            .child(
                ui::text(12., FontWeight::NORMAL, 0.45)
                    .mt(px(12.))
                    .line_height(px(18.))
                    .child(tr(
                        "Your password goes to Ely.by only; Horizon doesn't keep it.",
                    )),
            )
    }
}
