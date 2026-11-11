//! Sign in with Ely.by from the account card: a username, a masked password, and a two-factor
//! code once the account asks for one. The sign-in runs on its own thread. The password and the
//! code leave their fields when the form is submitted, and are never logged, kept after the
//! attempt, or shown in an error.

use super::*;
use crate::line_edit::{self, Key, LineEdit};
use std::sync::{Arc, Mutex};

type Slot = Arc<Mutex<Option<Result<account::Account, String>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Which {
    Username,
    Password,
    Code,
}

pub(super) struct ElyForm {
    open: bool,
    username: LineEdit,
    password: LineEdit,
    code: LineEdit,
    username_focus: FocusHandle,
    password_focus: FocusHandle,
    code_focus: FocusHandle,
    focused: Option<Which>,
    /// The account asked for a two-factor code: the code field shows.
    need_code: bool,
    busy: bool,
    /// An English message: the form shows it through `tr`.
    error: Option<String>,
    /// Where the sign-in thread leaves its result; `elyby_tick` takes it.
    pending: Option<Slot>,
}

impl ElyForm {
    pub(super) fn new(cx: &mut App) -> Self {
        let username_focus = cx.focus_handle().tab_index(0).tab_stop(true);
        let password_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let code_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        Self {
            open: false,
            username: LineEdit::new(String::new(), 64, 28),
            password: LineEdit::new(String::new(), 256, 28),
            code: LineEdit::new(String::new(), 16, 8),
            username_focus,
            password_focus,
            code_focus,
            focused: None,
            need_code: false,
            busy: false,
            error: None,
            pending: None,
        }
    }

    /// Whether a sign-in is running now (its thread holds the password).
    pub(super) fn busy(&self) -> bool {
        self.busy
    }

    fn edit(&mut self, which: Which) -> &mut LineEdit {
        match which {
            Which::Username => &mut self.username,
            Which::Password => &mut self.password,
            Which::Code => &mut self.code,
        }
    }

    fn line(&self, which: Which) -> &LineEdit {
        match which {
            Which::Username => &self.username,
            Which::Password => &self.password,
            Which::Code => &self.code,
        }
    }

    fn focus(&self, which: Which) -> &FocusHandle {
        match which {
            Which::Username => &self.username_focus,
            Which::Password => &self.password_focus,
            Which::Code => &self.code_focus,
        }
    }

    /// Records which field has the focus, for the field's border.
    pub(super) fn note_focus(&mut self, window: &Window) {
        self.focused = [Which::Username, Which::Password, Which::Code]
            .into_iter()
            .find(|w| self.focus(*w).is_focused(window));
    }

    /// Closes the form and drops everything typed into it.
    fn close(&mut self) {
        self.open = false;
        self.busy = false;
        self.pending = None;
        self.need_code = false;
        self.error = None;
        self.focused = None;
        drop(self.username.take());
        drop(self.password.take());
        drop(self.code.take());
    }
}

impl Launcher {
    pub(super) fn elyby_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.elyby.open = true;
        self.elyby.error = None;
        window.focus(self.elyby.focus(Which::Username), cx);
        cx.notify();
    }

    /// Opens the form for an Ely.by account that must sign in again, its username filled in.
    pub(super) fn elyby_reopen(&mut self, username: &str, cx: &mut Context<Self>) {
        self.elyby.open = true;
        self.elyby.error = None;
        self.elyby.username.set(username.to_owned());
        cx.notify();
    }

    /// Closes the form unless a sign-in is running; the Cancel button is disabled meanwhile.
    pub(super) fn elyby_cancel(&mut self, cx: &mut Context<Self>) {
        if !self.elyby.busy {
            self.elyby.close();
        }
        cx.notify();
    }

    /// Starts the sign-in on its own thread. The password and the code are taken out of their
    /// fields here, so they live only in the thread's arguments.
    pub(super) fn elyby_submit(&mut self, cx: &mut Context<Self>) {
        let form = &mut self.elyby;
        if !form.open || form.busy {
            return;
        }
        let username = form.username.text().trim().to_owned();
        let password = form.password.text().to_owned();
        let code = form.code.text().trim().to_owned();
        let problem = if username.is_empty() {
            Some("Enter your Ely.by username.")
        } else if password.is_empty() {
            Some("Enter your password.")
        } else if form.need_code && code.is_empty() {
            Some("Enter the two-factor code.")
        } else {
            None
        };
        if let Some(problem) = problem {
            form.error = Some(problem.to_owned());
            cx.notify();
            return;
        }
        let code = form.need_code.then_some(code);
        drop(form.password.take());
        drop(form.code.take());
        let slot: Slot = Arc::default();
        form.pending = Some(slot.clone());
        form.busy = true;
        form.error = None;
        let _ = std::thread::spawn(move || {
            let result = account::elyby_sign_in(&username, &password, code.as_deref());
            if let Ok(mut out) = slot.lock() {
                *out = Some(result);
            }
        });
        cx.notify();
    }

    /// Takes the sign-in's result when it has arrived. True when the view must refresh.
    pub(super) fn elyby_tick(&mut self) -> bool {
        let Some(slot) = self.elyby.pending.clone() else {
            return false;
        };
        let Some(result) = slot.lock().ok().and_then(|mut out| out.take()) else {
            return false;
        };
        self.elyby.pending = None;
        self.elyby.busy = false;
        match result {
            Ok(_) => {
                self.elyby.close();
                self.accounts = Accounts::load();
                self.account_error = None;
                self.load_profile_heads();
                self.load_auth_statuses(false);
                self.signed_in_now();
            }
            // The password was not kept: the user enters it again with the code.
            Err(e) if e == account::elyby::CODE_NEEDED => self.elyby.need_code = true,
            Err(e) => self.elyby.error = Some(e),
        }
        true
    }

    /// The Ely.by row's body: the button that opens the form, or the form itself.
    pub(super) fn elyby_body(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme;
        let form = &self.elyby;
        if !form.open {
            return div().mt(px(8.)).child(
                button("elyby-open", tr("Sign in with Ely.by"), Kind::Secondary, &t).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.elyby_open(window, cx);
                    }),
                ),
            );
        }
        let busy = form.busy;
        let mut col = div().flex().flex_col().gap(px(8.)).mt(px(8.)).w_full();
        col = col.child(elyby_field(
            form,
            cx,
            Which::Username,
            "Username",
            false,
            &t,
        ));
        col = col.child(elyby_field(form, cx, Which::Password, "Password", true, &t));
        if form.need_code {
            col = col.child(div().text_color(t.muted).child(tr(
                "Enter the code from your authenticator app, and your password again.",
            )));
            col = col.child(elyby_field(
                form,
                cx,
                Which::Code,
                "Two-factor code",
                false,
                &t,
            ));
        }
        if busy {
            col = col.child(div().text_color(t.muted).child(tr("Signing in…")));
        } else if let Some(error) = &form.error {
            col = col.child(div().text_color(t.danger).child(error.clone()));
        }
        let submit = button("elyby-submit", tr("Sign in"), Kind::Primary, &t)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.elyby_submit(cx)));
        let cancel = button("elyby-cancel", tr("Cancel"), Kind::Secondary, &t)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.elyby_cancel(cx)));
        let (submit, cancel) = if busy {
            (disabled(submit), disabled(cancel))
        } else {
            (submit, cancel)
        };
        col.child(div().flex().gap(px(8.)).child(submit).child(cancel))
    }
}

/// One text field of the form: focus on click, typing and Enter/Escape in `on_key_down`.
fn elyby_field(
    form: &ElyForm,
    cx: &mut Context<Launcher>,
    which: Which,
    placeholder: &'static str,
    mask: bool,
    t: &Theme,
) -> Stateful<Div> {
    let focused = form.focused == Some(which);
    let field = div()
        .id(SharedString::from(format!("elyby-{which:?}")))
        .track_focus(form.focus(which))
        .cursor_text()
        .w_full()
        .h(px(34.))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(if focused { t.accent } else { t.outline })
        .bg(t.well)
        .child(line_edit::content(
            form.line(which),
            t,
            focused,
            false,
            mask,
            placeholder,
        ))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            window.focus(this.elyby.focus(which), cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
            match line_edit::handle(this.elyby.edit(which), ev, cx) {
                Key::Pass => return,
                Key::Enter => this.elyby_submit(cx),
                Key::Escape => window.blur(cx),
                _ => {}
            }
            cx.stop_propagation();
            cx.notify();
        }));
    line_edit::mouse(field, cx, move |this: &mut Launcher| this.elyby.edit(which))
}
