//! Flat, crisp reusable UI controls built on Rune primitives.

use super::theme::{ty, Palette};
use super::Ui;
use rune::tok::motion;
use rune::{
    col, el, row, rule, text, text_input, Align, Color, Cursor, Drag, El, Phase, TextEdit, Tx,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    Default,
    Primary,
    Danger,
    Ghost,
}

/// Standard button (default, primary, danger, or ghost).
#[allow(clippy::too_many_arguments)]
pub fn button(
    p: Palette,
    key: impl Into<String>,
    label: &'static str,
    kind: BtnKind,
    height: f32,
    px: f32,
    disabled: bool,
    on_press: impl Fn(&mut Ui, &mut Tx) + 'static,
) -> El {
    let (fill, hover, border_w, border_c, fg) = match kind {
        BtnKind::Default => (p.surface, p.hover, 1.0, p.line_2, p.text),
        BtnKind::Primary => (
            p.accent,
            p.accent_hover,
            0.0,
            Color::TRANSPARENT,
            Color::WHITE,
        ),
        BtnKind::Danger => (p.surface, p.red_soft, 1.0, p.line_2, p.red),
        BtnKind::Ghost => (
            Color::TRANSPARENT,
            p.hover,
            0.0,
            Color::TRANSPARENT,
            p.muted,
        ),
    };

    let mut b = row()
        .key(key)
        .role("button")
        .label_static(label)
        .h(height)
        .px(px)
        .center()
        .radius(if height <= 26.0 { 6.0 } else { 7.0 })
        .fill(fill)
        .disabled(disabled)
        .child(
            text(label)
                .font(if height <= 26.0 {
                    ty::SUB_MED
                } else {
                    ty::LABEL
                })
                .color(fg)
                .a11y_hidden(),
        );

    if border_w > 0.0 {
        b = b.border(border_w, border_c);
    }
    if !disabled {
        b = b
            .hover(hover)
            .press(if kind == BtnKind::Primary {
                p.accent_hover
            } else {
                p.press
            })
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press_tx(on_press);
    } else {
        b = b.opacity(0.45);
    }
    b
}

/// Compact 25px action button (Copy, Jump, Re-send, Allow, Rescan, etc.).
pub fn xs_button(
    p: Palette,
    key: impl Into<String>,
    label: &'static str,
    kind: BtnKind,
    disabled: bool,
    on_press: impl Fn(&mut Ui, &mut Tx) + 'static,
) -> El {
    button(p, key, label, kind, 25.0, 9.0, disabled, on_press)
}

/// Flat toggle switch matching the design (34x19 pill + animated knob).
pub fn toggle(
    p: Palette,
    key: impl Into<String>,
    label: &'static str,
    on: bool,
    on_toggle: impl Fn(&mut Ui, &mut Tx) + 'static,
) -> El {
    let (w, h, k) = (34.0_f32, 19.0_f32, 15.0_f32);
    let inset = (h - k) * 0.5;
    el().key(key)
        .role("switch")
        .label_static(label)
        .checked(on)
        .size(w, h)
        .radius(h * 0.5)
        .fill(if on { p.accent } else { p.line_2 })
        .cursor(Cursor::Pointer)
        .focusable(true)
        .motion(motion::SNAPPY)
        .on_press_tx(on_toggle)
        .child(
            el().key("knob")
                .size(k, k)
                .absolute(if on { w - k - inset } else { inset }, inset)
                .radius(k * 0.5)
                .fill(Color::WHITE)
                .shadow(1.0, 2.5, p.knob_shadow)
                .a11y_hidden(),
        )
}

/// Multi-option flat segmented control.
pub fn segmented<T: Copy + PartialEq + 'static>(
    p: Palette,
    key: impl Into<String>,
    items: &[(T, &'static str)],
    current: T,
    small: bool,
    on_pick: impl Fn(&mut Ui, T, &mut Tx) + Copy + 'static,
) -> El {
    let pad = 2.0_f32;
    let h = if small { 24.0 } else { 26.0 };
    let px = if small { 8.0 } else { 12.0 };

    let mut strip = row()
        .key(key)
        .role("segmented")
        .padding(pad)
        .gap(2.0)
        .radius(8.0)
        .fill(p.seg_bg)
        .center_y();

    for &(val, name) in items {
        let active = val == current;
        let mut seg = row()
            .key(name)
            .role("segment")
            .label_static(name)
            .checked(active)
            .h(h)
            .px(px)
            .center()
            .radius(6.0)
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press_tx(move |s: &mut Ui, tx| on_pick(s, val, tx))
            .child(
                text(name)
                    .font(if small { ty::SUB_MED } else { ty::LABEL })
                    .color(if active { p.text } else { p.muted })
                    .a11y_hidden(),
            );
        if active {
            seg = seg.fill(p.seg_active).shadow(1.0, 2.0, p.shadow_sm);
        } else {
            seg = seg.hover(p.hover);
        }
        strip = strip.child(seg);
    }
    strip
}

/// Subtle rounded icon badge (26x26) placing every setting row title on an aligned vertical rail.
pub fn setting_icon_badge(p: Palette, glyph: El) -> El {
    el().size(26.0, 26.0)
        .radius(7.0)
        .fill(p.icon_bg)
        .border(1.0, p.icon_border)
        .center()
        .a11y_hidden()
        .child(glyph)
}

/// Interactive horizontal slider with leading icon and live value readout (supports drag, track click, Esc cancel).
#[allow(clippy::too_many_arguments)]
pub fn slider_row(
    p: Palette,
    key: &'static str,
    leading_icon: El,
    label: &'static str,
    value_text: String,
    norm: f32,
    on_set: impl Fn(&mut Ui, f32, bool) + Copy + 'static,
) -> El {
    let norm = norm.clamp(0.0, 1.0);
    let (w, h, k, bar) = (116.0_f32, 20.0_f32, 14.0_f32, 4.0_f32);
    let travel = w - k;
    let to_norm = move |x: f32| ((x - k * 0.5) / travel).clamp(0.0, 1.0);

    let slider = el()
        .key("slider")
        .role("slider")
        .label_static(label)
        .range_value(f64::from(norm), 0.0, 1.0)
        .a11y_value(value_text.clone())
        .size(w, h)
        .cursor(Cursor::Pointer)
        .on_drag(move |s: &mut Ui, d: &mut Drag, tx| match d.phase {
            Phase::Down => {
                d.data = norm;
                let knob = k * 0.5 + norm * travel;
                if (d.x - knob).abs() > k * 0.5 + 3.0 {
                    on_set(s, to_norm(d.x), false);
                }
            }
            Phase::Move => {
                let knob = k * 0.5 + d.data * travel;
                let grab = if (d.start_x - knob).abs() <= k * 0.5 + 3.0 {
                    d.start_x - knob
                } else {
                    0.0
                };
                on_set(s, to_norm(d.x - grab), false);
                tx.instant();
            }
            Phase::Up => {
                if d.cancelled() {
                    on_set(s, d.data, true);
                } else {
                    let knob = k * 0.5 + d.data * travel;
                    let grab = if (d.start_x - knob).abs() <= k * 0.5 + 3.0 {
                        d.start_x - knob
                    } else {
                        0.0
                    };
                    on_set(s, to_norm(d.x - grab), true);
                }
            }
        })
        .child(
            el().key("track")
                .size(w, bar)
                .absolute(0.0, (h - bar) * 0.5)
                .radius(bar * 0.5)
                .fill(p.line_2)
                .a11y_hidden(),
        )
        .child(
            el().key("fill")
                .size((k * 0.5 + norm * travel).clamp(0.0, w), bar)
                .absolute(0.0, (h - bar) * 0.5)
                .radius(bar * 0.5)
                .fill(p.accent)
                .a11y_hidden(),
        )
        .child(
            el().key("knob")
                .size(k, k)
                .absolute(norm * travel, (h - k) * 0.5)
                .radius(k * 0.5)
                .fill(Color::WHITE)
                .border(1.0, p.line_2)
                .shadow(1.0, 2.5, p.knob_shadow)
                .a11y_hidden(),
        );

    let left = row()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(9.0)
        .center_y()
        .child(setting_icon_badge(p, leading_icon))
        .child(
            text(label)
                .font(ty::BODY_MED)
                .color(p.text)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .truncate(),
        );

    row()
        .key(key)
        .w_full()
        .min_h(38.0)
        .px(11.0)
        .py(6.0)
        .gap(8.0)
        .center_y()
        .justify_between()
        .child(left)
        .child(slider)
        .child(
            text(value_text)
                .w(44.0)
                .font(ty::SUB)
                .color(p.muted)
                .align(Align::End),
        )
}

/// Surface card with a 1px hairline border and 10px radius.
pub fn card(p: Palette, key: impl Into<String>) -> El {
    col()
        .key(key)
        .w_full()
        .fill(p.surface)
        .radius(10.0)
        .border(1.0, p.line)
}

/// Card containing rows separated by crisp 1px horizontal dividers.
pub fn card_rows(
    p: Palette,
    key: impl Into<String>,
    rows: impl IntoIterator<Item = El>,
) -> El {
    let mut c = card(p, key);
    let mut iter = rows.into_iter().peekable();
    while let Some(r) = iter.next() {
        c = c.child(r);
        if iter.peek().is_some() {
            c = c.child(rule(1.0, p.line).a11y_hidden());
        }
    }
    c
}

/// Standard leading-icon + label + optional description + right-side control row inside a card.
pub fn setting_row(
    p: Palette,
    key: impl Into<String>,
    leading_icon: El,
    label: &str,
    desc: Option<&str>,
    control: El,
) -> El {
    let mut text_col = col()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(1.0)
        .child(text(label).font(ty::BODY_MED).color(p.text).truncate());
    if let Some(d) = desc {
        text_col = text_col.child(text(d).font(ty::SUB).color(p.muted).truncate());
    }
    let left = row()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(9.0)
        .center_y()
        .child(setting_icon_badge(p, leading_icon))
        .child(text_col);
    row()
        .key(key)
        .w_full()
        .min_h(if desc.is_some() { 46.0 } else { 38.0 })
        .px(11.0)
        .py(if desc.is_some() { 8.0 } else { 6.0 })
        .gap(8.0)
        .center_y()
        .justify_between()
        .child(left)
        .child(control)
}

/// Section header row above a card (e.g. "Connection", "Clients", "Pointer").
pub fn section_head(p: Palette, key: impl Into<String>, title: &str, right: Option<El>) -> El {
    let mut r = row()
        .key(key)
        .w_full()
        .h(24.0)
        .center_y()
        .justify_between()
        .gap(8.0)
        .child(text(title).font(ty::SECTION).color(p.muted));
    if let Some(extra) = right {
        r = r.child(extra);
    }
    r
}

/// Compact pill badge (e.g. "1 online", "Cursor: This PC", "Linux").
pub fn badge(
    p: Palette,
    key: impl Into<String>,
    label: impl Into<String>,
    live: bool,
    on: bool,
) -> El {
    let (bg, border, fg) = if live {
        (p.green_soft, Color::TRANSPARENT, p.green)
    } else if on {
        (p.canvas, p.line, p.text_2)
    } else {
        (p.canvas, p.line, p.muted)
    };
    let mut e = row()
        .key(key)
        .h(20.0)
        .px(8.0)
        .center()
        .radius(10.0)
        .fill(bg)
        .child(text(label).font(ty::BADGE).color(fg));
    if border != Color::TRANSPARENT {
        e = e.border(1.0, border);
    }
    e
}

/// Keyboard shortcut keycap chip (`Ctrl`, `Alt`, `Arrow`, `Scroll Lock`).
pub fn kbd(p: Palette, label: &str) -> El {
    row()
        .h(20.0)
        .px(5.0)
        .center()
        .radius(5.0)
        .fill(p.surface)
        .border(1.0, p.line_2)
        .child(text(label).font(ty::KBD).color(p.text_2))
}

/// Single-line bordered text input.
#[allow(clippy::too_many_arguments)]
pub fn text_field(
    p: Palette,
    key: &'static str,
    edit: &TextEdit,
    width: Option<f32>,
    placeholder: &'static str,
    mono: bool,
    access: impl for<'a> Fn(&'a mut Ui) -> &'a mut TextEdit + 'static,
    on_submit: Option<fn(&mut Ui, &mut Tx)>,
) -> El {
    let style = if mono { ty::MONO } else { ty::BODY };
    let mut input = text_input(edit, style)
        .key("input")
        .label_static(placeholder)
        .single_line()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(p.text)
        .placeholder(placeholder, p.faint)
        .caret(p.accent, p.accent.alpha(0.22))
        .on_edit(access);
    if let Some(sub) = on_submit {
        input = input.on_submit(sub);
    }

    let inner = row()
        .key("clip")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .center_y()
        .clip_rounded(0.0)
        .child(input);

    let mut box_el = row()
        .key(key)
        .h(28.0)
        .px(9.0)
        .center_y()
        .radius(6.0)
        .fill(p.surface)
        .border(1.0, if edit.focused { p.accent } else { p.line_2 })
        .cursor(Cursor::Text)
        .child(inner);

    box_el = match width {
        Some(w) => box_el.w(w),
        None => box_el.grow(1.0).shrink().min_w(0.0),
    };
    box_el
}

/// Single-line masked password input (uses Rune's `.secure()` so text is masked and omitted
/// from the OS accessibility tree).
pub fn password_field(
    p: Palette,
    key: &'static str,
    edit: &TextEdit,
    placeholder: &'static str,
    access: impl for<'a> Fn(&'a mut Ui) -> &'a mut TextEdit + 'static,
    on_submit: Option<fn(&mut Ui, &mut Tx)>,
) -> El {
    let mut input = text_input(edit, ty::BODY)
        .key("input")
        .label_static(placeholder)
        .single_line()
        .secure()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(p.text)
        .placeholder(placeholder, p.faint)
        .caret(p.accent, p.accent.alpha(0.22))
        .on_edit(access);
    if let Some(sub) = on_submit {
        input = input.on_submit(sub);
    }

    let inner = row()
        .key("clip")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .center_y()
        .clip_rounded(0.0)
        .child(input);

    row()
        .key(key)
        .w_full()
        .h(32.0)
        .px(10.0)
        .center_y()
        .radius(6.0)
        .fill(p.surface)
        .border(1.0, if edit.focused { p.accent } else { p.line_2 })
        .cursor(Cursor::Text)
        .child(inner)
}

/// Centered empty state inside a list card.
pub fn empty_box(
    p: Palette,
    key: impl Into<String>,
    top_icon: Option<El>,
    title: &str,
    sub: &str,
) -> El {
    let mut c = col()
        .key(key)
        .w_full()
        .grow(1.0)
        .padding(18.0)
        .gap(4.0)
        .center();
    if let Some(ico) = top_icon {
        c = c.child(el().mb(4.0).child(ico));
    }
    c.child(
        text(title)
            .font(ty::BODY_MED)
            .color(p.text_2)
            .align(Align::Center),
    )
    .child(
        text(sub)
            .w_full()
            .font(ty::SUB)
            .color(p.muted)
            .wrap()
            .align(Align::Center),
    )
}
