//! User consent before starting the privileged Linux input helper.
use crate::SettingsLanguage;
use gpui_kit::component::{WindowExt, dialog::DialogButtonProps};
use gpui_kit::{App, ParentElement, Window};
use std::rc::Rc;

pub fn show_linux_input_permission(
    language: SettingsLanguage,
    confirm: impl Fn(&mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let locale = language.catalog_locale();
    let title = bongocat_i18n::text(locale, "startup_permission.linux.title").to_owned();
    let description =
        bongocat_i18n::text(locale, "startup_permission.linux.description").to_owned();
    let confirm_label = bongocat_i18n::text(locale, "startup_permission.linux.confirm").to_owned();
    let later = bongocat_i18n::text(locale, "startup_permission.later").to_owned();
    let confirm = Rc::new(confirm);
    window.open_alert_dialog(cx, move |dialog, _, _| {
        let confirm = confirm.clone();
        dialog
            .confirm()
            .title(title.clone())
            .child(description.clone())
            .button_props(
                DialogButtonProps::default()
                    .ok_text(confirm_label.clone())
                    .cancel_text(later.clone())
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                confirm(cx);
                true
            })
    });
}
