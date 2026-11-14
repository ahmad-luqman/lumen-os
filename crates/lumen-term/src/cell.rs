//! What one character cell of the grid holds.

/// A cell's colour: the terminal's default (which the renderer picks), or an
/// index into the 256-colour palette ([`crate::palette`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
}

/// A cell's attributes (a bit set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(u8);

impl Flags {
    pub const NONE: Flags = Flags(0);
    pub const BOLD: Flags = Flags(1 << 0);
    /// Foreground and background swapped (SGR 7).
    pub const INVERSE: Flags = Flags(1 << 1);
    /// The left half of a two-column character; the cell to its right is a
    /// [`Flags::SPACER`].
    pub const WIDE: Flags = Flags(1 << 2);
    /// The right half of a two-column character (draw nothing but the
    /// background); the cell to its left is [`Flags::WIDE`].
    pub const SPACER: Flags = Flags(1 << 3);

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }

    pub const fn without(self, other: Flags) -> Flags {
        Flags(self.0 & !other.0)
    }
}

/// One character cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

impl Cell {
    /// An empty cell in the default colours.
    pub const BLANK: Cell = Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: Flags::NONE };

    /// An empty cell with background `bg` (what erasing leaves).
    pub const fn blank(bg: Color) -> Cell {
        Cell { ch: ' ', fg: Color::Default, bg, flags: Flags::NONE }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::BLANK
    }
}
