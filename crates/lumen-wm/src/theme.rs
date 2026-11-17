//! The desktop's two palettes (#154 switches between them: `theme
//! light|dark`, `lumen.theme=`). Colours are `0x00RRGGBB`, as `lumen-gfx`
//! draws them.

/// Every colour the desktop draws with, for one theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    /// The flat colour behind the windows when there is no wallpaper.
    pub desktop: u32,
    pub border_focused: u32,
    pub border: u32,
    pub title_focused: u32,
    pub title_focused_text: u32,
    pub title: u32,
    pub title_text: u32,
    pub close: u32,
    pub close_glyph: u32,
    /// A client area's background until its owner draws.
    pub client: u32,
    pub client_text: u32,
    /// Buttons and selections (#156).
    pub accent: u32,
    pub accent_text: u32,
    pub taskbar: u32,
    pub taskbar_text: u32,
    pub pointer_outline: u32,
    pub pointer_fill: u32,
}

impl Theme {
    pub const LIGHT: Theme = Theme {
        name: "light",
        desktop: 0x3a6ea5,
        border_focused: 0x2b5797,
        border: 0x9a9a9a,
        title_focused: 0x2b5797,
        title_focused_text: 0xffffff,
        title: 0xd6d6d6,
        title_text: 0x404040,
        close: 0xc42b1c,
        close_glyph: 0xffffff,
        client: 0xf3f3f3,
        client_text: 0x1a1a1a,
        accent: 0x0067c0,
        accent_text: 0xffffff,
        taskbar: 0xe6e6e6,
        taskbar_text: 0x1a1a1a,
        pointer_outline: 0x000000,
        pointer_fill: 0xffffff,
    };

    pub const DARK: Theme = Theme {
        name: "dark",
        desktop: 0x1e2430,
        border_focused: 0x4c8ed0,
        border: 0x3c3c3c,
        title_focused: 0x2d5c8a,
        title_focused_text: 0xffffff,
        title: 0x2b2b2b,
        title_text: 0xa0a0a0,
        close: 0xc42b1c,
        close_glyph: 0xffffff,
        client: 0x1f1f1f,
        client_text: 0xe6e6e6,
        accent: 0x4ca0e0,
        accent_text: 0x000000,
        taskbar: 0x181818,
        taskbar_text: 0xe6e6e6,
        pointer_outline: 0x000000,
        pointer_fill: 0xffffff,
    };

    /// Every theme, the default (light) first.
    pub const ALL: [&'static Theme; 2] = [&Theme::LIGHT, &Theme::DARK];

    /// The theme with this name.
    pub fn by_name(name: &str) -> Option<&'static Theme> {
        Theme::ALL.into_iter().find(|t| t.name == name)
    }

    /// Every colour with its field's name.
    pub fn colours(&self) -> [(&'static str, u32); 17] {
        [
            ("desktop", self.desktop),
            ("border_focused", self.border_focused),
            ("border", self.border),
            ("title_focused", self.title_focused),
            ("title_focused_text", self.title_focused_text),
            ("title", self.title),
            ("title_text", self.title_text),
            ("close", self.close),
            ("close_glyph", self.close_glyph),
            ("client", self.client),
            ("client_text", self.client_text),
            ("accent", self.accent),
            ("accent_text", self.accent_text),
            ("taskbar", self.taskbar),
            ("taskbar_text", self.taskbar_text),
            ("pointer_outline", self.pointer_outline),
            ("pointer_fill", self.pointer_fill),
        ]
    }

    /// The colours drawn on each other (foreground, background), each with
    /// the least contrast ratio it must have, in tenths (WCAG's: 4.5 for
    /// text, 3 for a glyph or an outline).
    pub fn pairs(&self) -> [(&'static str, u32, u32, u32); 7] {
        [
            ("title_focused_text on title_focused", self.title_focused_text, self.title_focused, 45),
            ("title_text on title", self.title_text, self.title, 45),
            ("close_glyph on close", self.close_glyph, self.close, 30),
            ("client_text on client", self.client_text, self.client, 45),
            ("accent_text on accent", self.accent_text, self.accent, 45),
            ("taskbar_text on taskbar", self.taskbar_text, self.taskbar, 45),
            ("pointer_outline on pointer_fill", self.pointer_outline, self.pointer_fill, 30),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2's relative luminance.
    fn luminance(c: u32) -> f64 {
        let lin = |v: u32| {
            let s = f64::from(v & 0xff) / 255.0;
            if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(c >> 16) + 0.7152 * lin(c >> 8) + 0.0722 * lin(c)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (l1, l2) = (luminance(a), luminance(b));
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }

    #[test]
    fn both_palettes_are_complete_and_readable() {
        for t in Theme::ALL {
            // Naming every field without `..`: a field added to `Theme`
            // fails to compile here until `colours` lists it too.
            let Theme {
                name: _,
                desktop,
                border_focused,
                border,
                title_focused,
                title_focused_text,
                title,
                title_text,
                close,
                close_glyph,
                client,
                client_text,
                accent,
                accent_text,
                taskbar,
                taskbar_text,
                pointer_outline,
                pointer_fill,
            } = *t;
            let fields = [
                desktop,
                border_focused,
                border,
                title_focused,
                title_focused_text,
                title,
                title_text,
                close,
                close_glyph,
                client,
                client_text,
                accent,
                accent_text,
                taskbar,
                taskbar_text,
                pointer_outline,
                pointer_fill,
            ];
            let colours = t.colours();
            assert_eq!(colours.map(|(_, c)| c), fields, "{}: colours() in field order", t.name);
            for (n, c) in colours {
                assert_eq!(c >> 24, 0, "{}.{n} has a top byte", t.name);
            }
            let mut names = colours.map(|(n, _)| n);
            names.sort_unstable();
            assert!(names.windows(2).all(|w| w[0] != w[1]), "{}: a colour named twice", t.name);
            for (what, fg, bg, least) in t.pairs() {
                let ratio = contrast(fg, bg);
                assert!(ratio * 10.0 >= f64::from(least), "{}: {what} is {ratio:.2}:1", t.name);
            }
            // A focused window's frame looks different from the others'.
            assert_ne!(t.title_focused, t.title, "{}", t.name);
            assert_ne!(t.border_focused, t.border, "{}", t.name);
        }
        assert_ne!(Theme::LIGHT.desktop, Theme::DARK.desktop);
        assert_ne!(Theme::LIGHT.client, Theme::DARK.client);
    }

    #[test]
    fn themes_are_found_by_name() {
        assert_eq!(Theme::by_name("light"), Some(&Theme::LIGHT));
        assert_eq!(Theme::by_name("dark"), Some(&Theme::DARK));
        assert_eq!(Theme::by_name("Dark"), None);
        assert_eq!(Theme::by_name(""), None);
        assert_eq!(Theme::ALL[0].name, "light");
    }
}
