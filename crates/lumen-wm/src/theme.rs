//! The desktop's two palettes (#154 switches between them: `theme
//! light|dark`, `lumen.theme=`). Colours are `0x00RRGGBB`, as `lumen-gfx`
//! draws them.

/// Which theme: the closed set the desktop switches between (`theme
/// light|dark`, `lumen.theme=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ThemeId {
    Light,
    Dark,
}

impl ThemeId {
    /// Every theme, the default (light) first, each at its
    /// [`ThemeId::index`].
    pub const ALL: [ThemeId; 2] = [ThemeId::Light, ThemeId::Dark];

    /// Its place in [`ThemeId::ALL`] (what the kernel keeps in a byte).
    pub const fn index(self) -> u8 {
        match self {
            ThemeId::Light => 0,
            ThemeId::Dark => 1,
        }
    }

    /// The theme at `index` in [`ThemeId::ALL`], `None` past its end.
    pub const fn from_index(index: u8) -> Option<ThemeId> {
        match index {
            0 => Some(ThemeId::Light),
            1 => Some(ThemeId::Dark),
            _ => None,
        }
    }

    /// The theme named `name` (`light`, `dark`; exactly).
    pub fn by_name(name: &str) -> Option<ThemeId> {
        ThemeId::ALL.into_iter().find(|id| id.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            ThemeId::Light => "light",
            ThemeId::Dark => "dark",
        }
    }
}

// Every theme at its index, both ways: a variant added out of order, or
// left out of one of the matches, fails to build here.
const _: () = {
    let mut i = 0;
    while i < ThemeId::ALL.len() {
        assert!(ThemeId::ALL[i].index() as usize == i);
        assert!(matches!(ThemeId::from_index(i as u8), Some(id) if id.index() as usize == i));
        i += 1;
    }
    assert!(ThemeId::from_index(ThemeId::ALL.len() as u8).is_none());
};

/// Every colour the desktop draws with, for one theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub id: ThemeId,
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
        id: ThemeId::Light,
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
        id: ThemeId::Dark,
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

    /// The palette of theme `id`.
    pub const fn get(id: ThemeId) -> &'static Theme {
        match id {
            ThemeId::Light => &Theme::LIGHT,
            ThemeId::Dark => &Theme::DARK,
        }
    }

    /// Every colour with its field's name.
    #[cfg(test)]
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
    #[cfg(test)]
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
        for t in ThemeId::ALL.map(Theme::get) {
            // Naming every field without `..`: a field added to `Theme`
            // fails to compile here, and the array below then needs it, and
            // the comparison with `colours()` makes that list it too.
            let Theme {
                id: _,
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
            assert_eq!(colours.map(|(_, c)| c), fields, "{}: colours() in field order", t.id.name());
            for (n, c) in colours {
                assert_eq!(c >> 24, 0, "{}.{n} has a top byte", t.id.name());
            }
            let mut names = colours.map(|(n, _)| n);
            names.sort_unstable();
            assert!(names.windows(2).all(|w| w[0] != w[1]), "{}: a colour named twice", t.id.name());
            for (what, fg, bg, least) in t.pairs() {
                let ratio = contrast(fg, bg);
                assert!(ratio * 10.0 >= f64::from(least), "{}: {what} is {ratio:.2}:1", t.id.name());
            }
            // A focused window's frame looks different from the others'.
            assert_ne!(t.title_focused, t.title, "{}", t.id.name());
            assert_ne!(t.border_focused, t.border, "{}", t.id.name());
        }
        assert_ne!(Theme::LIGHT.desktop, Theme::DARK.desktop);
        assert_ne!(Theme::LIGHT.client, Theme::DARK.client);
    }

    #[test]
    fn themes_are_found_by_name() {
        assert_eq!(ThemeId::by_name("light"), Some(ThemeId::Light));
        assert_eq!(ThemeId::by_name("dark"), Some(ThemeId::Dark));
        assert_eq!(ThemeId::by_name("Dark"), None);
        assert_eq!(ThemeId::by_name(""), None);
        assert_eq!(ThemeId::ALL[0], ThemeId::Light);
    }

    #[test]
    fn theme_ids_name_their_palettes() {
        for (i, id) in ThemeId::ALL.into_iter().enumerate() {
            assert_eq!(Theme::get(id).id, id);
            assert_eq!(ThemeId::by_name(id.name()), Some(id));
            assert_eq!(id.index() as usize, i);
            assert_eq!(ThemeId::from_index(id.index()), Some(id));
        }
        assert_eq!(ThemeId::from_index(2), None);
        assert_eq!(ThemeId::from_index(u8::MAX), None);
        assert_eq!(Theme::LIGHT.id.name(), "light");
        assert_eq!(Theme::DARK.id.name(), "dark");
    }
}
