//! The calculator (#207): a display and a pad of buttons in `lumen-ui`,
//! inside the app (the SDK's `ui::Toolkit`), so its window shows in `ui
//! tree` like a kernel window and `ui invoke` presses its buttons as a
//! click does. Keys work too, whichever button has the focus: lumen-calc's
//! characters (`0`-`9`, `.` or `,`, `+ - * /`, `x` or `×`, `÷`, `=`, `c`),
//! Enter (`=`), Backspace and Escape (`C`); Tab and Space go to the
//! toolkit. The arithmetic is `lumen_calc::Calc`'s (host-tested): left to
//! right, in exact decimals; after an error (dividing by zero, an
//! overflow) a digit or `.` starts again, as `C` does.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use lumen_app::ui::{Taken, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, KeyCode};
use lumen_calc::{Calc, Key, Op};
use lumen_ui::{App, Then, Ui, UiEvent, Widget};

lumen_app::manifest!(desktop);
lumen_app::main!(run);

/// Wide enough for the longest value the display shows,
/// `-142857142857142.714285714` (lumen-calc's `the_longest_result_shows_whole`;
/// the golden shows it).
const WIDTH: u32 = 300;
const HEIGHT: u32 = 300;
/// The display's width: the pad's.
const DISPLAY: u32 = WIDTH - 2 * 8;

/// The pad, row by row: each button's name (what `ui tree` shows, and its
/// label) and its key.
const PAD: [&[(&str, Key)]; 5] = [
    &[("7", Key::Digit(7)), ("8", Key::Digit(8)), ("9", Key::Digit(9)), ("/", Key::Op(Op::Div))],
    &[("4", Key::Digit(4)), ("5", Key::Digit(5)), ("6", Key::Digit(6)), ("*", Key::Op(Op::Mul))],
    &[("1", Key::Digit(1)), ("2", Key::Digit(2)), ("3", Key::Digit(3)), ("-", Key::Op(Op::Sub))],
    &[("0", Key::Digit(0)), (".", Key::Point), ("=", Key::Equals), ("+", Key::Op(Op::Add))],
    &[("C", Key::Clear), ("Back", Key::Backspace)],
];

/// The app: the calculator, its tree, and which button is which key.
struct Calculator {
    ui: Ui,
    calc: Calc,
    display: Widget,
    keys: Vec<(Widget, Key)>,
}

impl Calculator {
    fn new() -> Calculator {
        let mut ui = Ui::new();
        let calc = Calc::new();
        let display = ui.fixed_label(None, "Display", &calc.display(), DISPLAY);
        let pad = ui.vstack(None, "Pad");
        let mut keys = Vec::new();
        for (r, row) in PAD.iter().enumerate() {
            let line = ui.hstack(Some(pad), &format!("Row {}", r + 1));
            for &(name, key) in row.iter() {
                keys.push((ui.button(Some(line), name), key));
            }
        }
        Calculator { ui, calc, display, keys }
    }

    fn press(&mut self, key: Key) {
        self.calc.press(key);
        let shown = self.calc.display();
        self.ui.set_text(self.display, &shown);
    }
}

impl App for Calculator {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            if let UiEvent::Pressed(w) = e
                && let Some(&(_, key)) = self.keys.iter().find(|(b, _)| b == w)
            {
                self.press(key);
            }
        }
        Then::Carry
    }
}

/// The calculator key a key press stands for, if any.
fn key_of(code: KeyCode, text: Option<char>) -> Option<Key> {
    match code {
        KeyCode::Return | KeyCode::NumpadEnter => Some(Key::Equals),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Escape => Some(Key::Clear),
        _ => text.and_then(Key::from_char),
    }
}

/// The app's exit code: 0 once its window is closed, or 1 once `app`'s
/// failure is logged.
fn run() -> i32 {
    lumen_app::report(app())
}

fn app() -> Result<(), (&'static str, Error)> {
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Calculator", WIDTH, HEIGHT).map_err(|e| ("no window", e))?;
    let mut tk = Toolkit::new(Calculator::new(), window).map_err(|e| ("no toolkit", e))?;
    // The calculator's keys go to it whichever button has the focus; the
    // others (Tab, Space) to the toolkit.
    tk.run_with(|calc, e| {
        let Event::Key { code, action: KeyAction::Press { text }, .. } = *e else { return Taken::Pass };
        let Some(key) = key_of(code, text) else { return Taken::Pass };
        calc.press(key);
        Taken::Took
    })
    .map_err(|e| ("toolkit", e))
}
