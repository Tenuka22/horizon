//! The sign-in gate in the window (`horizon_launcher::account::launch`): the screen that asks for
//! an Ely.by account. It covers the launcher once for players without one, and again whenever
//! Play is refused for want of one. The Ely.by form (`elyby_form`) is inside it.
use super::*;

/// Why the gate asks, in the gate and in Settings' account card.
pub(super) const WHY: &str =
    "Horizon needs an Ely.by account to play online. It plays with your name and skin.";

impl Launcher {
    /// Whether the screen covers the window: nobody signed in, and either Play was refused or the
    /// player hasn't read it yet. The data question comes first.
    pub(crate) fn asking_sign_in(&self) -> bool {
        self.signed_in == Some(false)
            && (self.sign_in_screen_open || !self.settings.owner_notice_seen)
            && !self.asking_consent()
            && self.opening.is_none()
    }

    /// Opens the screen with the Ely.by form open on it.
    pub(crate) fn sign_in_to_play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sign_in_screen_open = true;
        self.elyby_open(window, cx);
    }

    /// An account is signed in: the screen is done, for good.
    pub(super) fn signed_in_now(&mut self) {
        self.signed_in = Some(true);
        self.sign_in_screen_open = false;
        self.client_error = None;
        if !self.settings.owner_notice_seen {
            self.settings.owner_notice_seen = true;
            self.save_settings();
        }
    }

    /// Closes it; it comes back with the next refused Play.
    fn close_sign_in_screen(&mut self, cx: &mut Context<Self>) {
        self.elyby_cancel(cx);
        self.sign_in_screen_open = false;
        if !self.settings.owner_notice_seen {
            self.settings.owner_notice_seen = true;
            self.save_settings();
        }
    }

    pub(crate) fn sign_in_screen(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        if crate::glass_ui::on() {
            return self.glass_sign_in_screen(cx);
        }
        let t = self.theme;
        let n = t.night;
        let body = div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                shell::text(theme::night::CAPTION, shell::c(n.muted))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("Ely.by account").to_string()),
            )
            .child(
                shell::text(theme::night::TITLE, shell::c(n.ink))
                    .mb(px(4.))
                    .child(tr("Sign in to play").to_string()),
            )
            .child(shell::text(theme::night::UI, shell::c(n.ink_soft)).child(tr(WHY).to_string()));
        let answers = div()
            .relative()
            .flex()
            .items_center()
            .gap(px(10.))
            .mt(px(14.))
            .child(
                shell::action("sign-in-later", shell::Action::Quiet, &t)
                    .child(shell::label(tr("Not now").to_string()))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.close_sign_in_screen(cx);
                        cx.notify();
                    })),
            );
        // Opaque: it sits over the page's content, not over the bare sky.
        let card = plate::cut(
            theme::night::PIXEL_CORNER,
            2,
            Some(plate::alpha(n.card, 0.98)),
            Some((plate::alpha(n.stroke, 2. * n.stroke_alpha), 1.)),
        )
        .flex()
        .flex_col()
        .gap(px(8.))
        // A set width: text measured at it wraps where it is drawn.
        .w(px(520.))
        .max_w_full()
        .p(px(32.))
        .child(body)
        .child(self.elyby_body(cx))
        .child(answers);
        // The sky stays behind, dimmed: the sheet is over the launcher, not instead of it.
        div()
            .id("sign-in-gate")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(32.))
            .py(px(24.))
            .bg(plate::alpha(n.page, 0.8))
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .occlude()
            .child(card)
    }

    /// The screen in the glass look: a dialog plate over the dimmed, blurred launcher (the
    /// mockup's modals), the Ely.by form in it.
    fn glass_sign_in_screen(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        use crate::glass_ui::{self as ui, white};
        use horizon_theme::glass::{ink, radius};
        let t = self.theme;
        let later = ui::button(
            "sign-in-later".into(),
            tr("Not now").into(),
            Kind::Ghost,
            &t,
        )
        .h(px(44.))
        .px(px(20.))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.close_sign_in_screen(cx);
            cx.notify();
        }));
        let card = ui::dialog_plate(radius::MODAL)
            .id("sign-in-card")
            .occlude()
            .flex()
            .flex_col()
            // A set width: text measured at it wraps where it is drawn.
            .w(px(520.))
            .max_w_full()
            .p(px(32.))
            .child(ui::text(13., FontWeight::MEDIUM, ink::QUIET).child(tr("Ely.by account")))
            .child(
                div()
                    .mt(px(8.))
                    .font_family(horizon_theme::glass::DISPLAY_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(28.))
                    .line_height(px(35.))
                    .text_color(white(ink::FULL))
                    .child(tr("Sign in to play")),
            )
            .child(
                ui::text(14., FontWeight::NORMAL, ink::BODY)
                    .mt(px(12.))
                    .child(tr(WHY)),
            )
            .child(self.elyby_body(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mt(px(24.))
                    .child(later),
            );
        div()
            .id("sign-in-gate")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .occlude()
            .child(ui::scrim_backdrop())
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(ui::scrim()),
            )
            .child(card)
    }
}
