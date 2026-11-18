//! #157's additions against the fixed-width fake text: removal (slots
//! never reused), menus and their items, the fixed-size label, bars (a
//! pinned widget, the rest hidden past it), and per-widget damage, checked
//! by a randomized model: repainting only the damage gives the pixels of
//! repainting everything.

use alloc::string::String;
use alloc::vec::Vec;

use lumen_gfx::{Rect, Surface};

use crate::node::{Action, InvokeError, Role, StateFlags};
use crate::text::{FixedText, Text};
use crate::ui::{Input, Key, Palette, Repaint, Ui, UiEvent, Widget, button_height};

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

fn centre(r: Rect) -> (i32, i32) {
    (r.x() + r.w() as i32 / 2, r.y() + r.h() as i32 / 2)
}

fn click_at(ui: &mut Ui, (x, y): (i32, i32)) -> Vec<UiEvent> {
    let mut events = ui.handle(Input::Down { x, y });
    events.extend(ui.handle(Input::Up { x, y }));
    events
}

fn names(ui: &Ui) -> Vec<(Role, String)> {
    ui.nodes().into_iter().map(|(_, n)| (n.role(), String::from(n.name()))).collect()
}

#[test]
fn a_removed_widget_names_nothing_and_its_slot_is_never_reused() {
    let mut ui = Ui::new();
    let row = ui.hstack(None, "Row");
    let a = ui.button(Some(row), "A");
    let b = ui.button(Some(row), "B");
    let inner = ui.vstack(Some(row), "Inner");
    let c = ui.text_field(Some(inner), "C", "text");
    ui.layout(AREA, &mut FixedText);
    ui.set_window_focused(true);
    let (a_id, b_id, c_id) = (
        ui.id(a).expect("not removed").get(),
        ui.id(b).expect("not removed").get(),
        ui.id(c).expect("not removed").get(),
    );
    // B focused and armed (pressed, not yet released), then removed.
    let at = centre(ui.bounds(b));
    let _ = ui.handle(Input::Down { x: at.0, y: at.1 });
    assert_eq!(ui.focused(), Some(b));
    let _ = (ui.take_changed(), ui.take_damage());
    let was = ui.bounds(b);
    assert!(ui.remove(b));
    assert!(!ui.remove(b), "removed once");
    assert!(ui.is_removed(b));
    assert_eq!(ui.focused(), None, "the focus goes with it");
    assert_eq!(ui.bounds(b), Rect::EMPTY);
    // What it covered (and its focus ring) is repainted; the tree changed.
    assert!(ui.take_changed());
    match ui.take_damage() {
        Repaint::Rects(r) => assert!(r.iter().any(|d| d.contains_rect(&was))),
        other => panic!("{other:?}"),
    }
    // The release where it was activates nothing (nothing is there now).
    assert!(ui.handle(Input::Up { x: at.0, y: at.1 }).is_empty());
    // Its id names nothing: not listed, not invoked, not hit.
    assert!(ui.nodes().iter().all(|(_, n)| n.id().get() != b_id));
    assert_eq!(ui.invoke(b_id, &Action::Press), Err(InvokeError::NoNode));
    assert!(ui.node(b_id).is_none());
    // Its setters do nothing, its accessors read empty.
    ui.set_text(b, "x");
    ui.set_disabled(b, true);
    assert!(!ui.is_disabled(b));
    assert!(!ui.take_changed());
    // Laid out again, A stays and the stack after it moves left.
    ui.layout(AREA, &mut FixedText);
    assert!(ui.hit(at.0, at.1).is_none_or(|(w, _)| w != b));
    // A stack removed takes its children with it.
    assert!(ui.remove(inner.widget()));
    assert!(ui.is_removed(c));
    assert_eq!(ui.text(c), None);
    assert_eq!(ui.invoke(c_id, &Action::SetValue(String::from("y"))), Err(InvokeError::NoNode));
    assert_eq!(names(&ui), [(Role::Group, String::from("Row")), (Role::Button, String::from("A"))]);
    // A new widget takes a new slot and a new id.
    let d = ui.button(Some(row), "D");
    assert!(d != b && d != c && d != inner.widget());
    assert!(ui.id(d).expect("not removed").get() > c_id);
    assert_eq!(ui.invoke(a_id, &Action::Press), Ok(alloc::vec![UiEvent::Pressed(a)]));
    // A child added to a removed stack is removed too.
    let e = ui.button(Some(inner), "E");
    assert!(ui.is_removed(e));
}

#[test]
fn a_freed_slot_is_taken_again_under_a_new_generation() {
    // A taskbar's buttons come and go with its windows: the tree doesn't
    // grow, and an old handle never reaches the widget in its slot now.
    let mut ui = Ui::bar(4);
    let row = ui.hstack(None, "Windows");
    let first = ui.button(Some(row), "First");
    let first_id = ui.id(first).expect("live").get();
    assert!(ui.remove(first));
    let second = ui.label(Some(row), "Second", "two");
    assert_ne!(first, second);
    assert!(ui.is_removed(first) && !ui.is_removed(second));
    assert_eq!(ui.id(first), None);
    assert_eq!(ui.text(first), None);
    ui.set_text(first, "changed through a stale handle");
    assert_eq!(ui.text(second), Some("two"));
    assert!(!ui.remove(first), "a stale handle removes nothing");
    assert!(!ui.is_removed(second));
    assert_eq!(ui.invoke(first_id, &Action::Press), Err(InvokeError::NoNode));
    assert!(ui.id(second).expect("live").get() > first_id, "a new node id");
    let slots = ui.slots();
    for n in 0..1000 {
        let b = ui.button(Some(row), &alloc::format!("Window {n}"));
        ui.layout(Rect::new(0, 0, 800, 40), &mut FixedText);
        assert!(ui.remove(b));
    }
    assert!(ui.slots() <= slots + 1, "{} slots after 1000 windows", ui.slots());
    // A child of a stack that is gone is born removed.
    assert!(ui.remove(row.widget()));
    let orphan = ui.button(Some(row), "Orphan");
    assert!(ui.is_removed(orphan));
    assert!(ui.nodes().is_empty());
}

#[test]
fn a_menu_shows_its_items_only_while_expanded_and_choosing_one_folds_it() {
    let mut ui = Ui::bar(4);
    let menu = ui.menu(None, "Launcher", &["Terminal", "Life"]);
    let after = ui.button(None, "After");
    ui.layout(Rect::new(0, 0, 600, 40), &mut FixedText);
    let menu_id = ui.id(menu).expect("not removed").get();
    let life = ui.item_id(menu, 1).expect("an item").get();
    // Folded: no items in the tree, and they name nothing.
    let node = ui.node(menu_id).expect("listed").1;
    assert_eq!((node.role(), node.state().contains(StateFlags::EXPANDED)), (Role::Menu, false));
    assert!(node.children().is_empty());
    assert_eq!(ui.invoke(life, &Action::Press), Err(InvokeError::NoNode));
    let folded = ui.bounds(after);
    // A press unfolds it: its items follow its head, the button after them.
    assert_eq!(ui.invoke(menu_id, &Action::Press), Ok(alloc::vec![UiEvent::Expanded(menu, true)]));
    ui.layout(Rect::new(0, 0, 600, 40), &mut FixedText);
    assert_eq!(ui.take_damage(), Repaint::All, "the button after it moved");
    let node = ui.node(menu_id).expect("listed").1;
    assert!(node.state().contains(StateFlags::EXPANDED));
    assert_eq!(node.children().len(), 2);
    let rows = ui.nodes();
    let items: Vec<_> = rows.iter().filter(|(_, n)| n.role() == Role::MenuItem).collect();
    assert_eq!(items.iter().map(|(_, n)| n.name()).collect::<Vec<_>>(), ["Terminal", "Life"]);
    let (head, term, life_b) = (node.bounds(), items[0].1.bounds(), items[1].1.bounds());
    assert!(head.right() < term.x() && term.right() < life_b.x() && life_b.right() < ui.bounds(after).x());
    assert!(ui.bounds(after).x() > folded.x());
    assert_eq!(items[0].1.actions(), [crate::node::ActionKind::Press]);
    // A click on an item and `press` on it do the same: chosen, folded.
    let mut clicked = ui.clone();
    assert_eq!(click_at(&mut clicked, centre(life_b)), [UiEvent::Chosen(menu, 1)]);
    assert_eq!(ui.invoke(life, &Action::Press), Ok(alloc::vec![UiEvent::Chosen(menu, 1)]));
    assert_eq!((ui.expanded(menu), clicked.expanded(menu)), (Some(false), Some(false)));
    // A click on the head toggles it; Enter on it, focused, too.
    assert_eq!(click_at(&mut ui, centre(head)), [UiEvent::Expanded(menu, true)]);
    assert_eq!(ui.focused(), Some(menu));
    assert_eq!(ui.handle(Input::Key(Key::Enter)), [UiEvent::Expanded(menu, false)]);
    // Folded by the owner, an item pressed but not released does nothing.
    let _ = ui.invoke(menu_id, &Action::Press);
    ui.layout(Rect::new(0, 0, 600, 40), &mut FixedText);
    let (x, y) = centre(ui.node(life).expect("shown").1.bounds());
    let _ = ui.handle(Input::Down { x, y });
    ui.set_expanded(menu, false);
    assert!(ui.handle(Input::Up { x, y }).is_empty());
    // A menu item takes only `press`.
    let _ = ui.invoke(menu_id, &Action::Press);
    assert_eq!(
        ui.invoke(life, &Action::Select),
        Err(InvokeError::NotAllowed(Role::MenuItem, crate::node::ActionKind::Select))
    );
}

#[test]
fn a_fixed_label_keeps_its_size_and_a_change_repaints_only_its_bounds() {
    let mut ui = Ui::bar(4);
    let left = ui.button(None, "Left");
    let clock = ui.fixed_label(None, "Clock", "12:34", 48);
    ui.pin_right(clock);
    let area = Rect::new(0, 0, 300, 32);
    ui.layout(area, &mut FixedText);
    let _ = (ui.take_changed(), ui.take_damage());
    let b = ui.bounds(clock);
    assert_eq!((b.w(), b.h(), b.right()), (48, FixedText::LINE, 300 - 4));
    assert_eq!(b.y(), (32 - FixedText::LINE as i32) / 2, "centred in the bar");
    let left_b = ui.bounds(left);
    for t in ["12:35", "1", "a much longer text than fits"] {
        ui.set_text(clock, t);
        ui.layout(area, &mut FixedText);
        assert!(ui.take_changed());
        assert_eq!(ui.take_damage(), Repaint::Rects(alloc::vec![b]), "{t}");
        assert_eq!((ui.bounds(clock), ui.bounds(left)), (b, left_b));
        // Painting it touches nothing outside its bounds.
        let mut s = Surface::new(300, 32);
        s.fill(0x123456);
        ui.paint_in(&mut s, &mut FixedText, &PALETTE, b);
        for y in 0..32 {
            for x in 0..300 {
                if !b.contains(x, y) {
                    assert_eq!(s.pixel(x, y), Some(0x123456), "({x}, {y}) {t}");
                }
            }
        }
    }
    // The same text again changes nothing.
    ui.set_text(clock, "a much longer text than fits");
    assert_eq!(ui.take_damage(), Repaint::Nothing);
}

#[test]
fn a_bar_pins_its_right_widget_and_hides_what_doesnt_fit_before_it() {
    let mut ui = Ui::bar(4);
    let row = ui.hstack(None, "Windows");
    let buttons: Vec<Widget> = (0..20).map(|n| ui.button(Some(row), &alloc::format!("Window {n}"))).collect();
    let clock = ui.fixed_label(None, "Clock", "00:00", 40);
    ui.pin_right(clock);
    let h = button_height(FixedText::LINE);
    let area = Rect::new(0, 0, 500, h + 8);
    ui.layout(area, &mut FixedText);
    let limit = ui.bounds(clock).x() - crate::ui::GAP as i32;
    let shown: Vec<Rect> = buttons.iter().map(|&b| ui.bounds(b)).filter(|r| !r.is_empty()).collect();
    assert!(!shown.is_empty() && shown.len() < buttons.len());
    // The ones shown come first and stay short of the clock; the rest are
    // hidden: not hit, not painted.
    assert!(buttons.iter().take(shown.len()).all(|&b| !ui.bounds(b).is_empty()));
    assert!(shown.iter().all(|r| r.right() <= limit && r.y() == 4));
    assert!(ui.bounds(row.widget()).right() <= limit);
    let hidden = buttons[shown.len()];
    assert!(ui.bounds(hidden).is_empty());
    // Closing two windows' buttons lets one more show.
    ui.remove(buttons[0]);
    ui.remove(buttons[1]);
    ui.layout(area, &mut FixedText);
    assert!(!ui.bounds(hidden).is_empty());
    let mut s = Surface::new(area.w(), area.h());
    ui.paint(&mut s, &mut FixedText, &PALETTE);
    // Across the clock's left gap nothing but the background is drawn.
    for y in 0..area.h() as i32 {
        assert_eq!(s.pixel(limit + 1, y), Some(PALETTE.background));
    }
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn repainting_only_the_damage_matches_repainting_everything() {
    let mut ops = [0u32; 12];
    for (n, seed) in [1u64, 0xdead_beef, 0x1234_5678, 42, 7, 0x5eed].into_iter().enumerate() {
        let mut rng = Rng(seed);
        let mut ui = if n % 2 == 0 { Ui::new() } else { Ui::bar(4) };
        let area = if n % 2 == 0 { Rect::new(0, 0, 360, 260) } else { Rect::new(0, 0, 640, 40) };
        let row = ui.hstack(None, "Row");
        let mut widgets: Vec<Widget> = alloc::vec![
            ui.menu(Some(row), "Menu", &["One", "Two", "Three"]),
            ui.button(Some(row), "Go"),
            ui.checkbox(Some(row), "Box", false),
        ];
        let col = ui.vstack(None, "Col");
        widgets.push(ui.text_field(Some(col), "Field", "ab"));
        widgets.push(ui.list(Some(col), "List", &["x", "y", "z"]));
        widgets.push(ui.label(Some(col), "Label", "hello"));
        let fixed = ui.fixed_label(None, "Clock", "00:00", 40);
        if n % 2 == 1 {
            ui.pin_right(fixed);
        }
        widgets.push(fixed);
        let mut text = FixedText;
        ui.layout(area, &mut text);
        let mut shown = Surface::new(area.w(), area.h());
        ui.paint(&mut shown, &mut text, &PALETTE);
        let _ = ui.take_damage();
        for step in 0..600 {
            let op = rng.below(12);
            ops[op] += 1;
            let live: Vec<Widget> = widgets.iter().copied().filter(|&w| !ui.is_removed(w)).collect();
            let pick = live.get(rng.below(live.len())).copied();
            match (op, pick) {
                (0 | 1, _) => {
                    // A click somewhere (often on a widget).
                    let (x, y) = match pick.filter(|_| rng.below(4) != 0) {
                        Some(w) if !ui.bounds(w).is_empty() => centre(ui.bounds(w)),
                        _ => (rng.below(area.w() as usize) as i32, rng.below(area.h() as usize) as i32),
                    };
                    let _ = click_at(&mut ui, (x, y));
                }
                (2, _) => {
                    let keys = [Key::Tab, Key::BackTab, Key::Enter, Key::Char(' '), Key::Char('q'), Key::Backspace];
                    let _ = ui.handle(Input::Key(keys[rng.below(keys.len())]));
                }
                (3, Some(w)) => {
                    let texts = ["", "a", "12:59", "a longer text", "zz"];
                    ui.set_text(w, texts[rng.below(texts.len())]);
                }
                (4, Some(w)) => ui.set_disabled(w, rng.below(2) == 0),
                (5, _) => ui.set_window_focused(rng.below(2) == 0),
                (6, Some(w)) if rng.below(4) == 0 => {
                    let _ = ui.remove(w);
                }
                (7, _) => {
                    let name = ["New", "Another", "W"][rng.below(3)];
                    widgets.push(ui.button(Some(row), name));
                }
                (8, Some(w)) => {
                    // Every action on a node of it (or an item).
                    let ids: Vec<u64> = ui.nodes().iter().map(|(_, n)| n.id().get()).collect();
                    let id = if rng.below(2) == 0 {
                        ui.id(w).expect("not removed").get()
                    } else {
                        ids[rng.below(ids.len())]
                    };
                    let actions = [Action::Press, Action::Focus, Action::Select, Action::SetValue(String::from("v"))];
                    let _ = ui.invoke(id, &actions[rng.below(actions.len())]);
                }
                (9, Some(w)) => ui.set_expanded(w, rng.below(2) == 0),
                (10, _) => {
                    let _ = ui.handle(Input::Cancel);
                }
                _ => {}
            }
            ui.layout(area, &mut text);
            let _ = ui.take_changed();
            match ui.take_damage() {
                Repaint::Nothing => {}
                Repaint::All => ui.paint(&mut shown, &mut text, &PALETTE),
                Repaint::Rects(rects) => {
                    for r in rects {
                        ui.paint_in(&mut shown, &mut text, &PALETTE, r);
                    }
                }
            }
            let mut full = Surface::new(area.w(), area.h());
            ui.paint(&mut full, &mut text, &PALETTE);
            assert!(shown == full, "seed {seed:#x} step {step} op {op}: the damage missed a change");
        }
    }
    assert!(ops.iter().take(11).all(|&n| n > 100), "{ops:?}");
}

#[test]
fn the_text_draw_is_clipped_by_paint_in() {
    // FixedText draws blocks; one drawn across the clip's edge stops there.
    let mut ui = Ui::new();
    let l = ui.label(None, "L", "wwwwwwww");
    ui.layout(AREA, &mut FixedText);
    let b = ui.bounds(l);
    let clip = Rect::new(b.x(), b.y(), b.w() / 2, b.h());
    let mut s = Surface::new(AREA.w(), AREA.h());
    ui.paint_in(&mut s, &mut FixedText, &PALETTE, clip);
    for x in clip.right()..b.right() {
        for y in b.y()..b.bottom() {
            assert_eq!(s.pixel(x, y), Some(0), "({x}, {y})");
        }
    }
    let mut t = FixedText;
    assert!(t.width("wwwwwwww") == b.w());
}
