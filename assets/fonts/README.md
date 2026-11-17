# Fonts

`AtkinsonHyperlegible-Regular.ttf`: Atkinson Hyperlegible Regular 1.006,
Copyright 2020 Braille Institute of America, Inc., under the SIL Open Font
License 1.1 (`AtkinsonHyperlegible-OFL.txt`). The desktop's UI font
(`crates/lumen-font`, which embeds it with `include_bytes!`).

| File | Source | sha256 |
|---|---|---|
| `AtkinsonHyperlegible-Regular.ttf` | <https://raw.githubusercontent.com/google/fonts/1b22086d13bdbbf5ec63d01a39a49246e4ac355f/ofl/atkinsonhyperlegible/AtkinsonHyperlegible-Regular.ttf> | `7fb917c89019896d0b52ee84b7cbb3304c18cb90b19a62f5e32712bd23e97669` |
| `AtkinsonHyperlegible-OFL.txt` | <https://raw.githubusercontent.com/google/fonts/1b22086d13bdbbf5ec63d01a39a49246e4ac355f/ofl/atkinsonhyperlegible/OFL.txt> | `f32d22b3908fcad2c86a74000614ec22e6a7f66ea7e867e616026a27aebdc143` |

`cargo test -p lumen-font` checks the embedded font's size, name, glyph
count and 64-bit FNV-1a hash (`0x323b1baa87635ac9`, `lumen_font::UI_FONT_FNV1A`),
so a changed file fails it.
