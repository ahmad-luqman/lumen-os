//! The toolkit against the fixed-width fake text: layout, hit testing,
//! dispatch, the tree and its ids, invoke's errors, invoke and a click
//! giving the same state, and the reply slot with an owner.

use alloc::string::String;
use alloc::vec::Vec;

use lumen_gfx::{Rect, Surface};

use crate::app::{App, Over, Repainted, Served, Then, repaint, serve};
use crate::counter::Counter;
use crate::node::{Action, ActionKind, InvokeError, Role, StateFlags, Value};
use crate::reply::{First, NotStarted, Reply};
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

/// Every node's raw id, in tree order.
fn ids(ui: &Ui) -> Vec<u64> {
    ui.nodes().iter().map(|(_, n)| n.id().get()).collect()
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
    assert_eq!(click(&mut c, |c| c.quit), Then::Close);
}

#[test]
fn a_disabled_widget_takes_no_click_key_or_action() {
    let mut c = laid_out();
    click(&mut c, |c| c.lock);
    assert_eq!(c.ui().checked(c.lock), Some(true));
    assert!(c.ui().is_disabled(c.add));
    click(&mut c, |c| c.add);
    let add = c.ui().id(c.add).expect("not removed").get();
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
    let ids = ids(c.ui());
    assert_eq!(c.ui().top_ids().iter().map(|i| i.get()).collect::<Vec<_>>(), [ids[0], ids[3], ids[7]]);
    assert_eq!(nodes[3].1.children().iter().map(|i| i.get()).collect::<Vec<_>>(), &ids[4..7]);
    // Unfocused, the window's focused widget doesn't say so.
    c.ui_mut().set_window_focused(false);
    assert!(c.ui().nodes().iter().all(|(_, n)| !n.state().contains(StateFlags::FOCUSED)));
}

#[test]
fn ids_stay_the_same_across_changes_and_repaints() {
    let mut c = laid_out();
    let before = ids(c.ui());
    let mut s = Surface::new(400, 200);
    for _ in 0..3 {
        click(&mut c, |c| c.add);
        click(&mut c, |c| c.lock);
        c.ui_mut().layout(AREA, &mut FixedText);
        c.ui().paint(&mut s, &mut FixedText, &PALETTE);
    }
    let after = ids(c.ui());
    assert_eq!(before, after);
    // Another tree's ids are new ones.
    let other = laid_out();
    let theirs = ids(other.ui());
    assert!(theirs.iter().all(|id| !before.contains(id)));
}

#[test]
fn invoke_errors_change_nothing() {
    let mut c = laid_out();
    let before = shown(c.ui());
    let label = c.ui().id(c.count_label).expect("not removed").get();
    let add = c.ui().id(c.add).expect("not removed").get();
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
        let id = invoked.ui().id(step(&invoked)).expect("not removed").get();
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
fn set_value_replaces_the_text_and_focus_focuses() {
    // `set_value` replaces a field's text as one edit (here "old"
    // goes), focusing the field; `focus` moves the focus.
    let mut ui = Ui::new();
    let field = ui.text_field(None, "Name", "old");
    let ok = ui.button(None, "OK");
    ui.layout(AREA, &mut FixedText);
    ui.set_window_focused(true);
    let id = ui.id(field).expect("not removed").get();
    assert_eq!(ui.invoke(id, &Action::SetValue(String::from("lumen"))), Ok(alloc::vec![UiEvent::Edited(field)]));
    assert_eq!(ui.text(field), Some("lumen"));
    assert_eq!(ui.focused(), Some(field));
    assert_eq!(ui.invoke(ui.id(ok).expect("not removed").get(), &Action::Focus), Ok(alloc::vec![UiEvent::Focused(ok)]));
    assert_eq!(ui.focused(), Some(ok));
}

/// What a window's owner does with a request: run it (the real [`serve`])
/// only if it can take it (`Reply::start`), then answer.
fn owner(c: &mut Counter, reply: &mut Reply<Served>, node: u64, action: &Action) {
    if reply.start().is_err() {
        return;
    }
    let served = serve(c, &mut FixedText, AREA, node, action);
    assert!(reply.finish(served));
}

#[test]
fn a_request_cancelled_at_its_deadline_then_reached_by_the_owner_leaves_the_state_unchanged() {
    // `ui invoke <Add> press`: the asker's deadline passes before the owner
    // takes the request (cancelled), the owner reaches it afterwards and
    // skips it: Add was not pressed. A retry then presses once, answering
    // Add's row.
    let mut c = laid_out();
    let add = c.ui().id(c.add).expect("not removed").get();
    let before = shown(c.ui());
    let mut reply = Reply::new();
    assert_eq!(reply.after_first_deadline(), First::Cancelled);
    owner(&mut c, &mut reply, add, &Action::Press);
    assert_eq!(shown(c.ui()), before);
    assert_eq!(c.count(), 0);
    assert_eq!(reply.start(), Err(NotStarted::Cancelled));
    let mut retry = Reply::new();
    owner(&mut c, &mut retry, add, &Action::Press);
    match retry.after_first_deadline() {
        First::Answered(Served::Row { depth, node }) => {
            assert_eq!((depth, node.id().get(), node.name()), (2, add, "Add"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(c.count(), 1);
}

#[test]
fn serve_answers_the_row_the_error_or_that_the_window_closed() {
    let mut c = laid_out();
    let count = c.ui().id(c.count_label).expect("not removed").get();
    let quit = c.ui().id(c.quit).expect("not removed").get();
    assert_eq!(
        serve(&mut c, &mut FixedText, AREA, count, &Action::Press),
        Served::Error(InvokeError::NotAllowed(Role::Label, ActionKind::Press))
    );
    assert_eq!(serve(&mut c, &mut FixedText, AREA, 0, &Action::Press), Served::Error(InvokeError::NoNode));
    assert_eq!(serve(&mut c, &mut FixedText, AREA, quit, &Action::Press), Served::ClosedItsWindow);
}

#[test]
fn an_action_that_removes_its_node_answers_that_it_ran() {
    // An app whose button's press removes the button (its handler builds a
    // new tree): it ran, so the answer says so rather than "changed
    // nothing" (a retry would press twice).
    struct Vanishing {
        ui: Ui,
        pressed: u32,
    }
    impl crate::app::App for Vanishing {
        fn ui_mut(&mut self) -> &mut Ui {
            &mut self.ui
        }
        fn apply(&mut self, events: &[UiEvent]) -> Then {
            if !events.is_empty() {
                self.pressed += 1;
                self.ui = Ui::new();
                self.ui.label(None, "Done", "yes");
            }
            Then::Carry
        }
    }
    let mut ui = Ui::new();
    let gone = ui.button(None, "Once");
    ui.layout(AREA, &mut FixedText);
    let id = ui.id(gone).expect("not removed").get();
    let mut app = Vanishing { ui, pressed: 0 };
    assert_eq!(serve(&mut app, &mut FixedText, AREA, id, &Action::Press), Served::RanNodeGone);
    assert_eq!(app.pressed, 1);
}

#[test]
fn every_action_a_widget_role_lists_reaches_a_handler() {
    // For each role a widget has, every action `Role::actions` lists runs
    // (no `NotAllowed`). The window root's actions are the kernel's
    // (`ui::root_action`, a ktest); `menu` and `menuitem` have no widget
    // yet.
    let mut ui = Ui::new();
    let button = ui.button(None, "B");
    let checkbox = ui.checkbox(None, "C", false);
    let field = ui.text_field(None, "T", "");
    let list = ui.list(None, "L", &["a", "b"]);
    ui.layout(AREA, &mut FixedText);
    let item = ui.nodes().iter().find(|(_, n)| n.role() == Role::ListItem).map(|(_, n)| n.id()).expect("an item");
    let nodes = [
        (Role::Button, ui.id(button).expect("not removed")),
        (Role::Checkbox, ui.id(checkbox).expect("not removed")),
        (Role::TextField, ui.id(field).expect("not removed")),
        (Role::List, ui.id(list).expect("not removed")),
        (Role::ListItem, item),
    ];
    for (role, id) in nodes {
        for &kind in role.actions() {
            let action = if kind == ActionKind::SetValue {
                Action::SetValue(String::from("v"))
            } else {
                Action::parse(kind.name(), &[]).expect("an action")
            };
            assert!(ui.invoke(id.get(), &action).is_ok(), "{role:?} {kind:?}");
        }
    }
    // Labels and stacks list none, and an action a role doesn't list is
    // refused.
    assert!(Role::Label.actions().is_empty() && Role::Group.actions().is_empty());
    assert_eq!(
        ui.invoke(ui.id(button).expect("not removed").get(), &Action::Select),
        Err(InvokeError::NotAllowed(Role::Button, ActionKind::Select))
    );
}

#[test]
fn every_role_and_action_kind_is_listed_once() {
    // `ALL` holds each variant once, in order (its order is also checked at
    // compile time), up to the last variant; every action any role lists
    // parses by its name (an `ActionKind` left out of `ALL` wouldn't), and
    // `NoSuchAction` names them all.
    assert_eq!(Role::ALL.len(), Role::MenuItem.index() + 1);
    assert_eq!(ActionKind::ALL.len(), ActionKind::Restore.index() + 1);
    for role in Role::ALL {
        for &kind in role.actions() {
            let args: &[&str] = if kind == ActionKind::SetValue { &["v"] } else { &[] };
            assert_eq!(Action::parse(kind.name(), args).map(|a| a.kind()), Ok(kind), "{role:?} {kind:?}");
        }
    }
    let said = alloc::format!("{}", InvokeError::NoSuchAction);
    assert!(ActionKind::ALL.iter().all(|k| said.contains(k.name())), "{said}");
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
    // Laid out again with nothing moved: nothing changed.
    c.ui_mut().layout(AREA, &mut FixedText);
    assert!(!c.ui_mut().take_changed());
    // A count that widens its label moves nothing else but the label.
    for _ in 0..10 {
        click(&mut c, |c| c.add);
    }
    let _ = c.ui_mut().take_changed();
    let before = c.ui().bounds(c.count_label);
    c.ui_mut().layout(AREA, &mut FixedText);
    assert!(c.ui_mut().take_changed(), "the label grew");
    assert!(c.ui().bounds(c.count_label).w() > before.w());
    c.ui_mut().invalidate();
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

#[test]
fn repaint_paints_what_changed_and_says_whether_to_publish() {
    // An app whose frame over the tree counts its paints: the first repaint
    // paints everything (and the tree changed: publish); then nothing, and
    // no frame painted; a press on Add repaints only its widgets.
    struct Framed(Counter, u32);
    impl App for Framed {
        fn ui_mut(&mut self) -> &mut Ui {
            self.0.ui_mut()
        }
        fn apply(&mut self, events: &[UiEvent]) -> Then {
            self.0.apply(events)
        }
        fn paint_over(&mut self, _: &mut Over<'_>) {
            self.1 += 1;
        }
    }
    let mut app = Framed(Counter::new(), 0);
    let mut s = Surface::new(AREA.w(), AREA.h());
    let first = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(first, Repainted { rects: alloc::vec![s.bounds()], changed: true });
    assert_eq!(app.1, 1);
    let idle = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(idle, Repainted { rects: Vec::new(), changed: false });
    assert_eq!(app.1, 1, "nothing painted, no frame over it");
    let add = app.0.ui().id(app.0.add).expect("live").get();
    let served = serve(&mut app, &mut FixedText, AREA, add, &Action::Press);
    assert!(matches!(served, Served::Row { .. }));
    let pressed = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert!(pressed.changed && !pressed.rects.is_empty());
    assert!(pressed.rects.iter().all(|r| *r != s.bounds()), "only the changed widgets: {:?}", pressed.rects);
    assert_eq!(app.1, 2);
}

#[test]
fn a_full_repaint_covers_the_surface_even_laid_out_in_less() {
    // The tree laid out in an area smaller than its surface: a full paint
    // still paints the whole surface (its background), so all of it is to
    // commit, not just the area.
    let mut app = Counter::new();
    let mut s = Surface::new(AREA.w() + 40, AREA.h() + 30);
    s.fill(0x123456);
    let first = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(first.rects, alloc::vec![s.bounds()]);
    let corner = (s.width() - 1, s.height() - 1);
    assert_eq!(s.pixel(corner.0 as i32, corner.1 as i32), Some(PALETTE.background), "painted past the area");
}

/// An app drawing a square of its own beside the Counter's tree: the tree
/// laid out in [`AREA`], the surface [`BOARD_W`] wider (the app's area).
/// Its frame paints only the square (its foreground) in the palette's
/// accent where the rectangles being repainted reach, recording each
/// call's rectangles; `drawn` is what it reports changed next.
struct Board {
    counter: Counter,
    square: Rect,
    drawn: Vec<Rect>,
    calls: Vec<Vec<Rect>>,
}

const BOARD_W: u32 = 200;

impl Board {
    fn new() -> Board {
        Board { counter: Counter::new(), square: Rect::new(450, 50, 20, 20), drawn: Vec::new(), calls: Vec::new() }
    }

    fn surface() -> Surface {
        Surface::new(AREA.w() + BOARD_W, AREA.h())
    }
}

impl App for Board {
    fn ui_mut(&mut self) -> &mut Ui {
        self.counter.ui_mut()
    }
    fn apply(&mut self, events: &[UiEvent]) -> Then {
        self.counter.apply(events)
    }
    fn paint_over(&mut self, over: &mut Over<'_>) {
        self.calls.push(over.rects.to_vec());
        for r in over.rects {
            if let Some(part) = r.intersect(&self.square) {
                over.surface.fill_rect(part, over.palette.accent);
            }
        }
    }
    fn take_drawn(&mut self) -> Vec<Rect> {
        core::mem::take(&mut self.drawn)
    }
}

#[test]
fn the_app_s_own_damage_is_repainted_and_committed_without_publishing() {
    let mut app = Board::new();
    let mut s = Board::surface();
    let first = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(first, Repainted { rects: alloc::vec![s.bounds()], changed: true });
    assert_eq!(app.calls, alloc::vec![alloc::vec![s.bounds()]], "a full repaint: everything over it");
    assert_eq!(s.pixel(455, 55), Some(PALETTE.accent));
    // Nothing changed: no frame.
    assert_eq!(repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA).rects, Vec::new());
    assert_eq!(app.calls.len(), 1);
    // The square moves: its old and new places are the app's damage,
    // committed and handed to its frame exactly, the tree unchanged (no
    // publish).
    let old = app.square;
    app.square = Rect::new(500, 120, 20, 20);
    app.drawn = alloc::vec![old, app.square];
    let moved = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(moved, Repainted { rects: alloc::vec![old, app.square], changed: false });
    assert_eq!(app.calls.last(), Some(&alloc::vec![old, app.square]));
    assert_eq!(s.pixel(505, 125), Some(PALETTE.accent), "drawn at its new place");
    assert_eq!(s.pixel(455, 55), Some(PALETTE.background), "its old place the tree's background: no trail");
    // Rectangles are cut to the surface; one wholly off it is dropped, and
    // with nothing else to repaint there's no frame at all.
    app.drawn = alloc::vec![Rect::new(-50, -50, 10, 10)];
    let calls = app.calls.len();
    assert_eq!(repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA).rects, Vec::new());
    assert_eq!(app.calls.len(), calls);
    app.drawn = alloc::vec![Rect::new(590, 190, 50, 50)];
    assert_eq!(
        repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA).rects,
        alloc::vec![Rect::new(590, 190, 10, 10)]
    );
}

#[test]
fn every_rect_repainted_gets_the_tree_s_paint_inside_its_area_or_not() {
    // A rectangle straddling the tree's area's right edge: the tree's
    // background on both sides (the app's frame paints only its square,
    // elsewhere), as a full repaint would leave it.
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    const JUNK: u32 = 0x00ab_cdef;
    let straddling = Rect::new(390, 150, 20, 20);
    s.fill_rect(straddling, JUNK);
    app.drawn = alloc::vec![straddling];
    let done = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(done.rects, alloc::vec![straddling]);
    assert_eq!(s.pixel(395, 155), Some(PALETTE.background), "inside the tree's area");
    assert_eq!(s.pixel(405, 155), Some(PALETTE.background), "outside it");
}

#[test]
fn the_app_s_rects_get_the_tree_s_paint_when_the_tree_changed_too() {
    // Junk straddling the area's edge, reported with a label change in the
    // same repaint: repainted as background (or tree) on both sides.
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    const JUNK: u32 = 0x00ab_cdef;
    let straddling = Rect::new(390, 150, 20, 20);
    s.fill_rect(straddling, JUNK);
    let add = app.counter.ui().id(app.counter.add).expect("live").get();
    assert!(matches!(serve(&mut app, &mut FixedText, AREA, add, &Action::Press), Served::Row { .. }));
    app.drawn = alloc::vec![straddling];
    let done = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert!(done.changed);
    let junk_left =
        (390..410).flat_map(|x| (150..170).map(move |y| (x, y))).filter(|&(x, y)| s.pixel(x, y) == Some(JUNK));
    assert_eq!(junk_left.count(), 0);
    let mut full = Board::surface();
    let mut again = Board::new();
    let add2 = again.counter.ui().id(again.counter.add).expect("live").get();
    let _ = serve(&mut again, &mut FixedText, AREA, add2, &Action::Press);
    repaint(&mut again, &mut full, &mut FixedText, &PALETTE, AREA);
    for x in 390..410 {
        for y in 150..170 {
            assert_eq!(s.pixel(x, y), full.pixel(x, y), "({x}, {y}) as a full repaint leaves it");
        }
    }
}

#[test]
fn the_tree_s_damage_and_the_app_s_are_committed_together() {
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let add = app.counter.ui().id(app.counter.add).expect("live").get();
    assert!(matches!(serve(&mut app, &mut FixedText, AREA, add, &Action::Press), Served::Row { .. }));
    app.drawn = alloc::vec![app.square];
    let both = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert!(both.changed, "the tree changed: publish");
    assert!(both.rects.contains(&app.square), "{:?}", both.rects);
    assert!(both.rects.len() > 1 && both.rects.iter().all(|r| *r != s.bounds()), "{:?}", both.rects);
    assert_eq!(app.calls.last(), Some(&both.rects), "the frame over all of it");
}

#[test]
fn a_new_palette_repaints_the_app_s_drawing_in_its_colours() {
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let dark = Palette { accent: 0x00ff8800, background: 0x202020, ..PALETTE };
    app.ui_mut().invalidate();
    let done = repaint(&mut app, &mut s, &mut FixedText, &dark, AREA);
    assert_eq!(done.rects, alloc::vec![s.bounds()]);
    assert_eq!(s.pixel(455, 55), Some(dark.accent));
}

#[test]
fn over_touches_says_whether_a_rect_meets_the_repainted_ones() {
    let mut s = Surface::new(10, 10);
    let rects = [Rect::new(0, 0, 4, 4), Rect::new(6, 6, 4, 4)];
    let over = Over { surface: &mut s, text: &mut FixedText, palette: &PALETTE, rects: &rects };
    assert!(over.touches(Rect::new(3, 3, 1, 1)));
    assert!(over.touches(Rect::new(9, 9, 5, 5)));
    assert!(!over.touches(Rect::new(4, 4, 2, 2)));
    assert!(!over.touches(Rect::EMPTY));
}

#[test]
fn overlapping_rects_are_painted_and_committed_once() {
    // The square (20 pixels) moved by half its width: its old and new
    // places overlap, committed as their union (600 pixels), not both (800).
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let old = app.square;
    app.square = Rect::new(old.x() + 10, old.y(), 20, 20);
    app.drawn = alloc::vec![old, app.square];
    let done = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    assert_eq!(done.rects, alloc::vec![Rect::new(old.x(), old.y(), 30, 20)]);
    let area: u64 = done.rects.iter().map(|r| r.area()).sum();
    assert_eq!(area, 600);
    // A rect inside another adds nothing.
    app.drawn = alloc::vec![Rect::new(450, 50, 40, 40), Rect::new(460, 60, 5, 5)];
    assert_eq!(
        repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA).rects,
        alloc::vec![Rect::new(450, 50, 40, 40)]
    );
}

#[test]
fn past_repaint_rects_they_merge_into_one_box() {
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    // One more apart than fit: all of them in one bounding box.
    let drawn: Vec<Rect> =
        (0..=crate::app::REPAINT_RECTS as i32).map(|i| Rect::new(420 + 10 * i, 10 * (i % 2), 2, 2)).collect();
    app.drawn = drawn.clone();
    let done = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let bounds = drawn.iter().fold(Rect::EMPTY, |acc, r| acc.union(r));
    assert_eq!(done.rects, alloc::vec![bounds]);
}

/// Whether `rects` are pairwise disjoint and cover exactly the pixels of
/// `wanted` (counted pixel by pixel over `within`).
fn disjoint_cover(rects: &[Rect], wanted: &[Rect], within: Rect) -> bool {
    let pairwise = rects.iter().enumerate().all(|(i, a)| rects[i + 1..].iter().all(|b| a.intersect(b).is_none()));
    let covered = |set: &[Rect], x: i32, y: i32| set.iter().any(|r| r.contains(x, y));
    pairwise
        && (within.y()..within.bottom())
            .all(|y| (within.x()..within.right()).all(|x| covered(rects, x, y) == covered(wanted, x, y)))
}

#[test]
fn crossing_rects_are_committed_as_their_union_once() {
    // A plus (a box around both would cost more than the two, so they
    // aren't merged) and a diagonal move: committed disjoint, their areas
    // adding up to the union's.
    let mut s = Board::surface();
    for (a, b, union) in [
        (Rect::new(420, 50, 60, 10), Rect::new(445, 30, 10, 50), 600 + 500 - 100),
        (Rect::new(450, 50, 20, 20), Rect::new(460, 60, 20, 20), 400 + 400 - 100),
    ] {
        let mut app = Board::new();
        repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
        app.drawn = alloc::vec![a, b];
        let done = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
        let area: u64 = done.rects.iter().map(|r| r.area()).sum();
        assert_eq!(area, union, "{:?}", done.rects);
        assert!(disjoint_cover(&done.rects, &[a, b], s.bounds()), "{:?}", done.rects);
        assert_eq!(app.calls.last(), Some(&done.rects), "the frame gets the disjoint ones");
    }
}

#[test]
fn an_app_drawing_once_per_rect_matches_a_full_repaint() {
    // Its frame darkens its square's pixels by half once per rectangle it
    // is handed (as blended text would): moved diagonally, the overlap is
    // darkened once, as a full repaint leaves it.
    struct Blender(Board);
    impl App for Blender {
        fn ui_mut(&mut self) -> &mut Ui {
            self.0.ui_mut()
        }
        fn apply(&mut self, events: &[UiEvent]) -> Then {
            self.0.apply(events)
        }
        fn paint_over(&mut self, over: &mut Over<'_>) {
            for r in over.rects {
                let Some(part) = r.intersect(&self.0.square) else { continue };
                for y in part.y()..part.bottom() {
                    for x in part.x()..part.right() {
                        let p = over.surface.pixel(x, y).expect("inside");
                        over.surface.fill_rect(Rect::new(x, y, 1, 1), (p >> 1) & 0x7f_7f7f);
                    }
                }
            }
        }
        fn take_drawn(&mut self) -> Vec<Rect> {
            self.0.take_drawn()
        }
    }
    let mut app = Blender(Board::new());
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let old = app.0.square;
    app.0.square = Rect::new(old.x() + 10, old.y() + 10, 20, 20);
    app.0.drawn = alloc::vec![old, app.0.square];
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let mut full = Board::surface();
    let mut again = Blender(Board::new());
    again.0.square = app.0.square;
    repaint(&mut again, &mut full, &mut FixedText, &PALETTE, AREA);
    assert_eq!(s.pixels(), full.pixels());
}

#[test]
fn a_tree_rect_and_an_app_rect_overlapping_are_committed_once() {
    // A press on Add damages its widgets; the app's rect overlaps the Add
    // button in the same repaint: committed disjoint, covering both.
    let mut app = Board::new();
    let mut s = Board::surface();
    repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let button = app.counter.ui().bounds(app.counter.add);
    let add = app.counter.ui().id(app.counter.add).expect("live").get();
    assert!(matches!(serve(&mut app, &mut FixedText, AREA, add, &Action::Press), Served::Row { .. }));
    let over_button = Rect::new(button.x() + 5, button.y() + 5, button.w(), button.h());
    app.drawn = alloc::vec![over_button];
    let done = repaint(&mut app, &mut s, &mut FixedText, &PALETTE, AREA);
    let pairwise =
        done.rects.iter().enumerate().all(|(i, a)| done.rects[i + 1..].iter().all(|b| a.intersect(b).is_none()));
    assert!(pairwise, "{:?}", done.rects);
    for r in [button, over_button] {
        let inside =
            (r.y()..r.bottom()).all(|y| (r.x()..r.right()).all(|x| done.rects.iter().any(|d| d.contains(x, y))));
        assert!(inside, "{r:?} not covered by {:?}", done.rects);
    }
}

/// A tree with a text area (an editor's buffer, its text the app's to
/// draw) and a button, laid out, the window focused, damage and changes
/// taken.
fn area_tree(text: &str) -> (Ui, Widget, Widget) {
    let mut ui = Ui::new();
    let area = ui.text_area(None, "Buffer", text, 200, 100);
    let ok = ui.button(None, "OK");
    ui.layout(AREA, &mut FixedText);
    ui.set_window_focused(true);
    let _ = ui.take_damage();
    let _ = ui.take_changed();
    (ui, area, ok)
}

#[test]
fn a_text_area_is_a_textfield_node_of_its_size_whose_text_the_tree_never_paints() {
    let (ui, area, _) = area_tree("hello");
    let b = ui.bounds(area);
    assert_eq!((b.w(), b.h()), (200, 100));
    let (_, node) = ui.nodes().into_iter().find(|(_, n)| n.name() == "Buffer").expect("its node");
    assert_eq!(node.role(), Role::TextField);
    assert_eq!(node.value(), Some(&Value::Text(String::from("hello"))));
    assert_eq!(node.bounds(), b);
    // Its field colour inside its border, and no text: every pixel inside
    // is the field's (a text field would show its text's blocks there).
    let mut s = Surface::new(400, 200);
    ui.paint(&mut s, &mut FixedText, &PALETTE);
    for y in b.y() + 1..b.bottom() - 1 {
        for x in b.x() + 1..b.right() - 1 {
            assert_eq!(s.pixel(x, y), Some(PALETTE.field), "({x}, {y})");
        }
    }
    assert_eq!(s.pixel(b.x(), b.y()), Some(PALETTE.border));
}

#[test]
fn setting_a_text_area_s_text_changes_the_tree_but_repaints_nothing() {
    let (mut ui, area, _) = area_tree("hello");
    ui.set_text(area, "hello, world");
    assert_eq!(ui.text(area), Some("hello, world"));
    assert!(ui.take_changed(), "the tree changed: to publish");
    assert_eq!(ui.take_damage(), crate::ui::Repaint::Nothing, "its app draws the text");
    // The same text again changes nothing.
    ui.set_text(area, "hello, world");
    assert!(!ui.take_changed());
}

#[test]
fn keys_never_edit_a_text_area_and_a_click_only_focuses_it() {
    let (mut ui, area, _) = area_tree("abc");
    let (x, y) = centre(ui.bounds(area));
    let mut events = ui.handle(Input::Down { x, y });
    events.extend(ui.handle(Input::Up { x, y }));
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(ui.focused(), Some(area));
    assert!(ui.handle(Input::Key(Key::Char('d'))).is_empty());
    assert!(ui.handle(Input::Key(Key::Backspace)).is_empty());
    assert_eq!(ui.text(area), Some("abc"));
}

#[test]
fn set_value_on_a_text_area_sets_its_text_and_reports_it_edited() {
    let (mut ui, area, _) = area_tree("old");
    let id = ui.id(area).expect("not removed").get();
    assert_eq!(ui.invoke(id, &Action::SetValue(String::from("new text"))), Ok(alloc::vec![UiEvent::Edited(area)]));
    assert_eq!(ui.text(area), Some("new text"));
    assert_eq!(ui.focused(), Some(area));
    assert!(ui.take_changed());
}

#[test]
fn a_text_area_s_focus_changing_repaints_only_its_ring() {
    let (mut ui, area, ok) = area_tree("text");
    let b = ui.bounds(area);
    let only_ring = |ui: &mut Ui| match ui.take_damage() {
        crate::ui::Repaint::Rects(rects) => {
            assert!(!rects.is_empty());
            for r in &rects {
                assert!(r.intersect(&b).is_none(), "{r:?} inside the area {b:?}");
            }
            rects
        }
        other => panic!("{other:?}"),
    };
    // Focused: its ring's strips (all four sides, around it), not its inside.
    ui.invoke(ui.id(area).expect("not removed").get(), &Action::Focus).expect("focusable");
    let strips = only_ring(&mut ui);
    let covered: u64 = strips.iter().map(|r| r.area()).sum();
    let ring_area = Rect::new(b.x() - 2, b.y() - 2, b.w() + 4, b.h() + 4).area() - b.area();
    assert_eq!(covered, ring_area, "{strips:?}");
    // The window losing the focus, and gaining it again.
    ui.set_window_focused(false);
    only_ring(&mut ui);
    ui.set_window_focused(true);
    only_ring(&mut ui);
    // The focus moving to the button: the area's ring, and the button.
    ui.invoke(ui.id(ok).expect("not removed").get(), &Action::Focus).expect("focusable");
    match ui.take_damage() {
        crate::ui::Repaint::Rects(rects) => {
            assert!(rects.iter().all(|r| r.intersect(&b).is_none()), "{rects:?}");
            assert!(rects.iter().any(|r| r.intersect(&ui.bounds(ok)).is_some()), "{rects:?}");
        }
        other => panic!("{other:?}"),
    }
}
