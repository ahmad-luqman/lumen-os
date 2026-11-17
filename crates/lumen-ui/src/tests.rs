//! The toolkit against the fixed-width fake text: layout, hit testing,
//! dispatch, the tree and its ids, invoke's errors, invoke and a click
//! giving the same state, and the reply slot with an owner.

use alloc::string::String;
use alloc::vec::Vec;

use lumen_gfx::{Rect, Surface};

use crate::counter::{Counter, Then};
use crate::node::{Action, ActionKind, InvokeError, Role, StateFlags, Value};
use crate::reply::{NotStarted, Outcome, Reply};
use crate::text::{FixedText, Text};
use crate::ui::{GAP, Input, Key, PAD, Palette, Ui, UiEvent, Widget};

const AREA: Rect = Rect::new(0, 0, 400, 200);

const PALETTE: Palette = Palette {
    background: 0xf3f3f3,
    text: 0x1a1a1a,
    accent: 0x0067c0,
    accent_text: 0xffffff,
    border: 0x9a9a9a,
    disabled: 0xa0a0a0,
    field: 0xffffff,
};

fn laid_out() -> Counter {
    let mut c = Counter::new();
    c.ui_mut().layout(AREA, &mut FixedText);
    c.ui_mut().set_window_focused(true);
    c
}

fn centre(r: Rect) -> (i32, i32) {
    (r.x() + r.w() as i32 / 2, r.y() + r.h() as i32 / 2)
}

/// A click (press and release) at the centre of `w`.
fn click(c: &mut Counter, pick: impl Fn(&Counter) -> Widget) -> Then {
    let (x, y) = centre(c.ui().bounds(pick(c)));
    let mut events = c.ui_mut().handle(Input::Down { x, y });
    events.extend(c.ui_mut().handle(Input::Up { x, y }));
    c.apply(&events)
}

/// What a tree shows, without its ids: role, name, value, state, bounds.
fn shown(ui: &Ui) -> Vec<(u32, Role, String, Option<Value>, StateFlags, Rect)> {
    ui.nodes()
        .into_iter()
        .map(|(d, n)| (d, n.role(), String::from(n.name()), n.value().cloned(), n.state(), n.bounds()))
        .collect()
}

#[test]
fn layout_places_each_widget_at_its_preferred_size() {
    // Fixed-width text: 8 px a character, 16-px lines. The top row (the
    // caption and the count) starts at the padding; the buttons' row below
    // it; each button its name plus 12 a side, 6 above and below.
    let c = laid_out();
    let ui = c.ui();
    let pad = PAD as i32;
    let count = ui.bounds(c.count_label);
    // "Count:" is 6 characters: 48 px, then the gap.
    assert_eq!(count, Rect::new(pad + 48 + GAP as i32, pad, 8, 16));
    let add = ui.bounds(c.add);
    assert_eq!(add, Rect::new(pad, pad + 16 + GAP as i32, 3 * 8 + 24, 16 + 12));
    let reset = ui.bounds(c.reset);
    assert_eq!(reset.x(), add.right() + GAP as i32);
    assert_eq!((reset.w(), reset.h()), (5 * 8 + 24, 28));
    let lock = ui.bounds(c.lock);
    assert_eq!(lock.x(), reset.right() + GAP as i32);
    assert_eq!(lock.w(), 14 + 6 + 4 * 8);
    let quit = ui.bounds(c.quit);
    assert_eq!(quit.y(), add.bottom() + GAP as i32);
    // Nothing overlaps.
    let leaves = [c.count_label, c.add, c.reset, c.lock, c.quit];
    for (i, a) in leaves.iter().enumerate() {
        for b in &leaves[i + 1..] {
            assert!(ui.bounds(*a).intersect(&ui.bounds(*b)).is_none(), "{a:?} overlaps {b:?}");
        }
    }
}

#[test]
fn hit_testing_finds_the_widget_and_list_row_under_a_point() {
    let c = laid_out();
    let ui = c.ui();
    for w in [c.add, c.reset, c.lock, c.quit, c.count_label] {
        let (x, y) = centre(ui.bounds(w));
        assert_eq!(ui.hit(x, y), Some((w, None)));
    }
    assert_eq!(ui.hit(399, 199), None, "the background");
    let gap = ui.bounds(c.add).right() + 2;
    assert_eq!(ui.hit(gap, ui.bounds(c.add).y() + 4), None, "between two buttons");
    // A list's rows.
    let mut ui = Ui::new();
    let list = ui.list(None, "Fruit", &["apple", "pear", "plum"]);
    ui.layout(AREA, &mut FixedText);
    let b = ui.bounds(list);
    let row_h = b.h() as i32 / 3;
    assert_eq!(ui.hit(b.x() + 2, b.y() + row_h + 1), Some((list, Some(1))));
    assert_eq!(ui.hit(b.x() + 2, b.y() + 2 * row_h + 1), Some((list, Some(2))));
}

#[test]
fn a_click_presses_only_where_it_went_down_and_came_up() {
    let mut c = laid_out();
    assert_eq!(click(&mut c, |c| c.add), Then::Carry);
    assert_eq!(c.count(), 1);
    assert_eq!(c.ui().text(c.count_label), Some("1"));
    // Down on Add, up on Reset: nothing.
    let (ax, ay) = centre(c.ui().bounds(c.add));
    let (rx, ry) = centre(c.ui().bounds(c.reset));
    assert!(c.ui_mut().handle(Input::Down { x: ax, y: ay }).is_empty());
    assert!(c.ui_mut().handle(Input::Up { x: rx, y: ry }).is_empty());
    assert_eq!(c.count(), 1);
    // A press forgotten (focus lost, events lost): its release does nothing.
    c.ui_mut().handle(Input::Down { x: ax, y: ay });
    c.ui_mut().handle(Input::Cancel);
    assert!(c.ui_mut().handle(Input::Up { x: ax, y: ay }).is_empty());
    assert_eq!(click(&mut c, |c| c.reset), Then::Carry);
    assert_eq!(c.count(), 0);
    assert_eq!(click(&mut c, |c| c.quit), Then::Quit);
}

#[test]
fn a_disabled_widget_takes_no_click_key_or_action() {
    let mut c = laid_out();
    click(&mut c, |c| c.lock);
    assert_eq!(c.ui().checked(c.lock), Some(true));
    assert!(c.ui().is_disabled(c.add));
    click(&mut c, |c| c.add);
    let add = c.ui().id(c.add).get();
    assert_eq!(c.ui_mut().invoke(add, &Action::Press), Err(InvokeError::Disabled));
    assert_eq!(c.count(), 0);
    // Tab skips it.
    for _ in 0..6 {
        let events = c.ui_mut().handle(Input::Key(Key::Tab));
        assert_ne!(events, [UiEvent::Focused(c.add)]);
    }
    click(&mut c, |c| c.lock);
    click(&mut c, |c| c.add);
    assert_eq!(c.count(), 1);
}

#[test]
fn keys_go_to_the_focused_widget_and_tab_moves_the_focus() {
    let mut c = laid_out();
    // Tab: Add first (the caption and count take no focus).
    assert_eq!(c.ui_mut().handle(Input::Key(Key::Tab)), [UiEvent::Focused(c.add)]);
    let events = c.ui_mut().handle(Input::Key(Key::Char(' ')));
    c.apply(&events);
    let events = c.ui_mut().handle(Input::Key(Key::Enter));
    c.apply(&events);
    assert_eq!(c.count(), 2);
    assert_eq!(c.ui_mut().handle(Input::Key(Key::Tab)), [UiEvent::Focused(c.reset)]);
    assert_eq!(c.ui_mut().handle(Input::Key(Key::Tab)), [UiEvent::Focused(c.lock)]);
    assert_eq!(c.ui_mut().handle(Input::Key(Key::Tab)), [UiEvent::Focused(c.quit)]);
    assert_eq!(c.ui_mut().handle(Input::Key(Key::Tab)), [UiEvent::Focused(c.add)], "wrapping");
    assert_eq!(c.ui_mut().handle(Input::Key(Key::BackTab)), [UiEvent::Focused(c.quit)]);
    // A text field takes printable text and Backspace.
    let mut ui = Ui::new();
    let field = ui.text_field(None, "Name", "");
    ui.layout(AREA, &mut FixedText);
    ui.handle(Input::Key(Key::Tab));
    for c in ['h', 'i', '\u{3}', '!'] {
        ui.handle(Input::Key(Key::Char(c)));
    }
    assert_eq!(ui.text(field), Some("hi!"), "a control character isn't inserted");
    ui.handle(Input::Key(Key::Backspace));
    assert_eq!(ui.text(field), Some("hi"));
}

#[test]
fn the_tree_lists_each_widget_with_its_role_value_and_state() {
    let mut c = laid_out();
    click(&mut c, |c| c.add);
    let nodes = c.ui().nodes();
    let rows: Vec<(u32, &str, &str, String, String)> = nodes
        .iter()
        .map(|(d, n)| {
            let value = n.value().map_or(String::from("-"), |v| alloc::format!("{v}"));
            (*d, n.role().name(), n.name(), value, alloc::format!("{}", n.state()))
        })
        .collect();
    assert_eq!(
        rows,
        [
            (1, "group", "Display", String::from("-"), String::from("-")),
            (2, "label", "Caption", String::from("Count:"), String::from("-")),
            (2, "label", "Count", String::from("1"), String::from("-")),
            (1, "group", "Controls", String::from("-"), String::from("-")),
            (2, "button", "Add", String::from("-"), String::from("focused")),
            (2, "button", "Reset", String::from("-"), String::from("-")),
            (2, "checkbox", "Lock", String::from("unchecked"), String::from("-")),
            (1, "button", "Quit", String::from("-"), String::from("-")),
        ]
    );
    // A group's children are its widgets' ids; the top ids are the root's.
    let ids: Vec<u64> = nodes.iter().map(|(_, n)| n.id().get()).collect();
    assert_eq!(c.ui().top_ids().iter().map(|i| i.get()).collect::<Vec<_>>(), [ids[0], ids[3], ids[7]]);
    assert_eq!(nodes[3].1.children().iter().map(|i| i.get()).collect::<Vec<_>>(), &ids[4..7]);
    // Unfocused, the window's focused widget doesn't say so.
    c.ui_mut().set_window_focused(false);
    assert!(c.ui().nodes().iter().all(|(_, n)| !n.state().contains(StateFlags::FOCUSED)));
}

#[test]
fn ids_stay_the_same_across_changes_and_repaints() {
    let mut c = laid_out();
    let before: Vec<u64> = c.ui().nodes().iter().map(|(_, n)| n.id().get()).collect();
    let mut s = Surface::new(400, 200);
    for _ in 0..3 {
        click(&mut c, |c| c.add);
        click(&mut c, |c| c.lock);
        c.ui_mut().layout(AREA, &mut FixedText);
        c.ui().paint(&mut s, &mut FixedText, &PALETTE);
    }
    let after: Vec<u64> = c.ui().nodes().iter().map(|(_, n)| n.id().get()).collect();
    assert_eq!(before, after);
    // Another tree's ids are new ones.
    let other = laid_out();
    let theirs: Vec<u64> = other.ui().nodes().iter().map(|(_, n)| n.id().get()).collect();
    assert!(theirs.iter().all(|id| !before.contains(id)));
}

#[test]
fn invoke_errors_change_nothing() {
    let mut c = laid_out();
    let before = shown(c.ui());
    let label = c.ui().id(c.count_label).get();
    let add = c.ui().id(c.add).get();
    let ui = c.ui_mut();
    assert_eq!(ui.invoke(u64::MAX, &Action::Press), Err(InvokeError::NoNode));
    assert_eq!(ui.invoke(label, &Action::Press), Err(InvokeError::NotAllowed(Role::Label, ActionKind::Press)));
    assert_eq!(
        ui.invoke(add, &Action::SetValue(String::from("9"))),
        Err(InvokeError::NotAllowed(Role::Button, ActionKind::SetValue))
    );
    assert_eq!(ui.invoke(add, &Action::Close), Err(InvokeError::NotAllowed(Role::Button, ActionKind::Close)));
    assert_eq!(shown(c.ui()), before);
    // Disabled.
    click(&mut c, |c| c.lock);
    let before = shown(c.ui());
    assert_eq!(c.ui_mut().invoke(add, &Action::Press), Err(InvokeError::Disabled));
    assert_eq!(shown(c.ui()), before);
}

#[test]
fn invoke_and_a_click_give_the_same_state() {
    // Two identical apps: one clicked, one invoked, step by step; their
    // trees show the same thing after each (ids aside).
    let mut clicked = laid_out();
    let mut invoked = laid_out();
    let steps =
        [|c: &Counter| c.add, |c: &Counter| c.add, |c: &Counter| c.lock, |c: &Counter| c.reset, |c: &Counter| c.lock];
    for step in steps {
        let w = step(&clicked);
        click(&mut clicked, |_| w);
        let id = invoked.ui().id(step(&invoked)).get();
        let events = invoked.ui_mut().invoke(id, &Action::Press).expect("pressable");
        invoked.apply(&events);
        assert_eq!(shown(clicked.ui()), shown(invoked.ui()));
    }
    // A list item: `select`, `press` and a click on its row agree.
    let mut a = Ui::new();
    let la = a.list(None, "Fruit", &["apple", "pear"]);
    let mut b = a.clone();
    a.layout(AREA, &mut FixedText);
    b.layout(AREA, &mut FixedText);
    let row = a.nodes()[2].1.bounds();
    let (x, y) = centre(row);
    a.handle(Input::Down { x, y });
    assert_eq!(a.handle(Input::Up { x, y }), [UiEvent::Selected(la, 1)]);
    let item = b.nodes()[2].1.id().get();
    assert_eq!(b.invoke(item, &Action::Select), Ok(alloc::vec![UiEvent::Selected(la, 1)]));
    assert_eq!(shown(&a), shown(&b));
    assert_eq!(a.nodes()[0].1.value(), Some(&Value::Selected(Some(1))));
    assert!(a.nodes()[2].1.state().contains(StateFlags::SELECTED));
    assert!(!a.nodes()[1].1.state().contains(StateFlags::SELECTED));
}

#[test]
fn set_value_and_focus_act_like_typing_and_clicking() {
    let mut ui = Ui::new();
    let field = ui.text_field(None, "Name", "");
    let ok = ui.button(None, "OK");
    ui.layout(AREA, &mut FixedText);
    ui.set_window_focused(true);
    let id = ui.id(field).get();
    assert_eq!(ui.invoke(id, &Action::SetValue(String::from("lumen"))), Ok(alloc::vec![UiEvent::Edited(field)]));
    assert_eq!(ui.text(field), Some("lumen"));
    assert_eq!(ui.focused(), Some(field));
    assert_eq!(ui.invoke(ui.id(ok).get(), &Action::Focus), Ok(alloc::vec![UiEvent::Focused(ok)]));
    assert_eq!(ui.focused(), Some(ok));
}

/// What a window's owner does with a request: run it only if it can take
/// it (`Reply::start`), then answer.
fn serve(c: &mut Counter, reply: &mut Reply<Result<Value, InvokeError>>, node: u64, action: &Action) {
    if reply.start().is_err() {
        return;
    }
    let result = c.ui_mut().invoke(node, action).map(|events| {
        c.apply(&events);
        Value::Text(String::from(c.ui().text(c.count_label).unwrap_or("")))
    });
    reply.finish(result);
}

#[test]
fn a_request_cancelled_at_its_deadline_then_reached_by_the_owner_leaves_the_state_unchanged() {
    // `ui invoke <Add> press`: the asker's deadline passes before the owner
    // takes the request (cancelled), the owner reaches it afterwards and
    // skips it: Add was not pressed. A retry then presses once.
    let mut c = laid_out();
    let add = c.ui().id(c.add).get();
    let before = shown(c.ui());
    let mut reply = Reply::new();
    assert_eq!(reply.after_deadline(), Outcome::Cancelled);
    serve(&mut c, &mut reply, add, &Action::Press);
    assert_eq!(shown(c.ui()), before);
    assert_eq!(c.count(), 0);
    assert_eq!(reply.start(), Err(NotStarted::Cancelled));
    let mut retry = Reply::new();
    serve(&mut c, &mut retry, add, &Action::Press);
    assert_eq!(retry.after_deadline(), Outcome::Answered(Ok(Value::Text(String::from("1")))));
    assert_eq!(c.count(), 1);
}

#[test]
fn painting_draws_each_widget_inside_its_bounds() {
    let mut c = laid_out();
    let mut s = Surface::new(400, 200);
    c.ui().paint(&mut s, &mut FixedText, &PALETTE);
    let add = c.ui().bounds(c.add);
    // The button's corner is the accent, its name's first block the
    // accent's text, the background untouched far away.
    assert_eq!(s.pixel(add.x() + 1, add.y() + 1), Some(PALETTE.accent));
    let name_x = add.x() + 12 + 1;
    assert_eq!(s.pixel(name_x + 1, add.y() + 6 + 3 + 1), Some(PALETTE.accent_text));
    assert_eq!(s.pixel(399, 199), Some(PALETTE.background));
    // Disabled, the button is the disabled colour.
    click(&mut c, |c| c.lock);
    c.ui().paint(&mut s, &mut FixedText, &PALETTE);
    assert_eq!(s.pixel(add.x() + 1, add.y() + 1), Some(PALETTE.disabled));
    // Its count's text is the count.
    let count = c.ui().bounds(c.count_label);
    assert_eq!(s.pixel(count.x() + 2, count.y() + 4), Some(PALETTE.text));
    // The fake measures as it draws.
    assert_eq!(FixedText.width("abc"), 24);
}

#[test]
fn changes_are_reported_once() {
    let mut c = laid_out();
    assert!(c.ui_mut().take_changed());
    assert!(!c.ui_mut().take_changed());
    click(&mut c, |c| c.add);
    assert!(c.ui_mut().take_changed());
    assert!(!c.ui_mut().take_changed());
    // A click on nothing changes nothing.
    c.ui_mut().handle(Input::Down { x: 399, y: 199 });
    c.ui_mut().handle(Input::Up { x: 399, y: 199 });
    assert!(!c.ui_mut().take_changed());
}
