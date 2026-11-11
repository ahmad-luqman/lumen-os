//! Minimal, allocation-free ELF64 (little-endian) reader.
//!
//! The kernel uses this to symbolize addresses in crash reports: Limine hands
//! the kernel its own ELF file, and [`SymbolTable::lookup`] maps an
//! instruction pointer to `function+offset`. It must work with no heap and in
//! a crashed kernel, so it only borrows the input bytes, never panics on
//! malformed input, and every read is bounds-checked.
//!
//! The crate is `no_std` and has host-side unit tests (`cargo test -p lumen-elf`).

#![no_std]

use core::fmt;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

/// Section type of a full symbol table.
pub const SHT_SYMTAB: u32 = 2;
/// Symbol type: function / code.
pub const STT_FUNC: u8 = 2;

const EHDR_SIZE: usize = 64;
const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;

/// Why an ELF image could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Shorter than an ELF header, or a table points past the end of the file.
    Truncated,
    /// Missing `\x7fELF` magic.
    BadMagic,
    /// Not a 64-bit little-endian image.
    Unsupported,
    /// No `SHT_SYMTAB` section (the binary was stripped).
    NoSymbolTable,
    /// A header field is inconsistent (e.g. a wrong entry size).
    Malformed,
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ElfError::Truncated => "truncated ELF image",
            ElfError::BadMagic => "not an ELF image",
            ElfError::Unsupported => "not an ELF64 little-endian image",
            ElfError::NoSymbolTable => "no symbol table (stripped?)",
            ElfError::Malformed => "malformed ELF header",
        };
        f.write_str(s)
    }
}

fn read_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off.checked_add(2)?)?.try_into().ok()?))
}
fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}
fn read_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off.checked_add(8)?)?.try_into().ok()?))
}

/// One section header (only the fields we need).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub name_offset: u32,
    pub kind: u32,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub entsize: u64,
}

/// A borrowed, validated ELF64 image.
#[derive(Clone, Copy)]
pub struct Elf<'a> {
    bytes: &'a [u8],
    shoff: usize,
    shnum: usize,
}

impl<'a> Elf<'a> {
    /// Validate the ELF header and section-header table bounds.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::Truncated);
        }
        if bytes[0..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != ELFCLASS64 || bytes[5] != ELFDATA2LSB {
            return Err(ElfError::Unsupported);
        }
        let shoff = read_u64(bytes, 0x28).ok_or(ElfError::Truncated)? as usize;
        let shentsize = read_u16(bytes, 0x3a).ok_or(ElfError::Truncated)? as usize;
        let shnum = read_u16(bytes, 0x3c).ok_or(ElfError::Truncated)? as usize;
        if shnum != 0 && shentsize != SHDR_SIZE {
            return Err(ElfError::Malformed);
        }
        let end = shnum.checked_mul(SHDR_SIZE).and_then(|n| n.checked_add(shoff)).ok_or(ElfError::Malformed)?;
        if end > bytes.len() {
            return Err(ElfError::Truncated);
        }
        Ok(Elf { bytes, shoff, shnum })
    }

    /// Number of section headers.
    pub fn section_count(&self) -> usize {
        self.shnum
    }

    /// Section header `index`, if it exists.
    pub fn section(&self, index: usize) -> Option<Section> {
        if index >= self.shnum {
            return None;
        }
        let o = self.shoff + index * SHDR_SIZE;
        let b = self.bytes;
        Some(Section {
            name_offset: read_u32(b, o)?,
            kind: read_u32(b, o + 4)?,
            addr: read_u64(b, o + 0x10)?,
            offset: read_u64(b, o + 0x18)?,
            size: read_u64(b, o + 0x20)?,
            link: read_u32(b, o + 0x28)?,
            entsize: read_u64(b, o + 0x38)?,
        })
    }

    /// The file bytes covered by `section`, if they lie inside the image.
    pub fn section_data(&self, section: &Section) -> Option<&'a [u8]> {
        let start = usize::try_from(section.offset).ok()?;
        let len = usize::try_from(section.size).ok()?;
        self.bytes.get(start..start.checked_add(len)?)
    }

    /// The `.symtab` symbol table and its string table.
    pub fn symbol_table(&self) -> Result<SymbolTable<'a>, ElfError> {
        let symtab = (0..self.shnum)
            .filter_map(|i| self.section(i))
            .find(|s| s.kind == SHT_SYMTAB)
            .ok_or(ElfError::NoSymbolTable)?;
        if symtab.entsize as usize != SYM_SIZE {
            return Err(ElfError::Malformed);
        }
        let strtab = self.section(symtab.link as usize).ok_or(ElfError::Malformed)?;
        Ok(SymbolTable {
            symbols: self.section_data(&symtab).ok_or(ElfError::Truncated)?,
            strings: self.section_data(&strtab).ok_or(ElfError::Truncated)?,
        })
    }
}

/// One symbol table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol<'a> {
    /// Raw (possibly mangled) symbol name.
    pub name: &'a str,
    pub value: u64,
    pub size: u64,
    pub kind: u8,
}

/// A borrowed `.symtab` + `.strtab` pair.
#[derive(Clone, Copy)]
pub struct SymbolTable<'a> {
    symbols: &'a [u8],
    strings: &'a [u8],
}

impl<'a> SymbolTable<'a> {
    /// Number of entries (including the null symbol at index 0).
    pub fn len(&self) -> usize {
        self.symbols.len() / SYM_SIZE
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry `index`, if it exists and its name is valid UTF-8.
    pub fn get(&self, index: usize) -> Option<Symbol<'a>> {
        let o = index.checked_mul(SYM_SIZE)?;
        let b = self.symbols;
        let name_off = read_u32(b, o)? as usize;
        let info = *b.get(o + 4)?;
        Some(Symbol {
            name: self.string_at(name_off)?,
            value: read_u64(b, o + 8)?,
            size: read_u64(b, o + 16)?,
            kind: info & 0xf,
        })
    }

    fn string_at(&self, off: usize) -> Option<&'a str> {
        let rest = self.strings.get(off..)?;
        let len = rest.iter().position(|&c| c == 0)?;
        core::str::from_utf8(&rest[..len]).ok()
    }

    /// Iterate over all readable entries.
    pub fn iter(&self) -> impl Iterator<Item = Symbol<'a>> + '_ {
        (0..self.len()).filter_map(|i| self.get(i))
    }

    /// Find the function containing `addr`.
    ///
    /// Returns the symbol and the offset of `addr` within it. A function
    /// symbol matches when `value <= addr < value + size`; zero-sized function
    /// symbols (common for hand-written assembly) match as a fallback when no
    /// sized symbol contains the address, using the closest one below `addr`.
    pub fn lookup(&self, addr: u64) -> Option<(Symbol<'a>, u64)> {
        let mut fallback: Option<Symbol<'a>> = None;
        for sym in self.iter() {
            if sym.kind != STT_FUNC || sym.name.is_empty() || sym.value > addr {
                continue;
            }
            if sym.size != 0 {
                if addr - sym.value < sym.size {
                    return Some((sym, addr - sym.value));
                }
            } else if fallback.is_none_or(|f| sym.value > f.value) {
                fallback = Some(sym);
            }
        }
        fallback.map(|s| (s, addr - s.value))
    }

    /// Address of the first function symbol whose raw name contains `needle`.
    pub fn find_function(&self, needle: &str) -> Option<Symbol<'a>> {
        self.iter().find(|s| s.kind == STT_FUNC && s.name.contains(needle))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{vec, vec::Vec};

    /// Build a minimal ELF64 image: null section, .symtab, .strtab.
    fn build_elf(symbols: &[(&str, u64, u64, u8)]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; SYM_SIZE]; // null symbol
        for (name, value, size, kind) in symbols {
            let name_off = strtab.len() as u32;
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            let mut e = [0u8; SYM_SIZE];
            e[0..4].copy_from_slice(&name_off.to_le_bytes());
            e[4] = (1 << 4) | kind; // STB_GLOBAL
            e[8..16].copy_from_slice(&value.to_le_bytes());
            e[16..24].copy_from_slice(&size.to_le_bytes());
            symtab.extend_from_slice(&e);
        }
        let symtab_off = EHDR_SIZE;
        let strtab_off = symtab_off + symtab.len();
        let shoff = strtab_off + strtab.len();

        let mut out = vec![0u8; EHDR_SIZE];
        out[0..4].copy_from_slice(&ELF_MAGIC);
        out[4] = ELFCLASS64;
        out[5] = ELFDATA2LSB;
        out[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&symtab);
        out.extend_from_slice(&strtab);

        let shdr = |kind: u32, off: usize, size: usize, link: u32, entsize: u64| {
            let mut h = [0u8; SHDR_SIZE];
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[0x18..0x20].copy_from_slice(&(off as u64).to_le_bytes());
            h[0x20..0x28].copy_from_slice(&(size as u64).to_le_bytes());
            h[0x28..0x2c].copy_from_slice(&link.to_le_bytes());
            h[0x38..0x40].copy_from_slice(&entsize.to_le_bytes());
            h
        };
        out.extend_from_slice(&[0u8; SHDR_SIZE]);
        out.extend_from_slice(&shdr(SHT_SYMTAB, symtab_off, symtab.len(), 2, SYM_SIZE as u64));
        out.extend_from_slice(&shdr(3, strtab_off, strtab.len(), 0, 0));
        out
    }

    #[test]
    fn looks_up_sized_function() {
        let img = build_elf(&[("alpha", 0x1000, 0x20, STT_FUNC), ("beta", 0x1020, 0x10, STT_FUNC)]);
        let syms = Elf::parse(&img).unwrap().symbol_table().unwrap();
        let (s, off) = syms.lookup(0x1024).unwrap();
        assert_eq!((s.name, off), ("beta", 4));
        let (s, off) = syms.lookup(0x1000).unwrap();
        assert_eq!((s.name, off), ("alpha", 0));
    }

    #[test]
    fn address_past_sized_function_uses_zero_size_fallback() {
        let img = build_elf(&[("alpha", 0x1000, 0x10, STT_FUNC), ("asm_stub", 0x2000, 0, STT_FUNC)]);
        let syms = Elf::parse(&img).unwrap().symbol_table().unwrap();
        assert!(syms.lookup(0x1800).is_none());
        let (s, off) = syms.lookup(0x2008).unwrap();
        assert_eq!((s.name, off), ("asm_stub", 8));
    }

    #[test]
    fn ignores_non_function_symbols() {
        let img = build_elf(&[("DATA", 0x1000, 0x100, 1)]);
        let syms = Elf::parse(&img).unwrap().symbol_table().unwrap();
        assert!(syms.lookup(0x1010).is_none());
    }

    #[test]
    fn find_function_by_substring() {
        let img = build_elf(&[("_ZN5lumen4kmon5crash17h0123E", 0x4000, 8, STT_FUNC)]);
        let syms = Elf::parse(&img).unwrap().symbol_table().unwrap();
        assert_eq!(syms.find_function("kmon5crash").unwrap().value, 0x4000);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert_eq!(Elf::parse(&[]).err(), Some(ElfError::Truncated));
        assert_eq!(Elf::parse(&[0u8; 64]).err(), Some(ElfError::BadMagic));
        let mut img = build_elf(&[("a", 1, 1, STT_FUNC)]);
        img.truncate(img.len() - 10); // cut into the section headers
        assert_eq!(Elf::parse(&img).err(), Some(ElfError::Truncated));
    }

    #[test]
    fn corrupt_offsets_do_not_panic() {
        let mut img = build_elf(&[("a", 1, 1, STT_FUNC)]);
        // Point the symtab section far past the end of the image.
        let shoff = u64::from_le_bytes(img[0x28..0x30].try_into().unwrap()) as usize;
        let sym_shdr = shoff + SHDR_SIZE;
        img[sym_shdr + 0x18..sym_shdr + 0x20].copy_from_slice(&u64::MAX.to_le_bytes());
        let elf = Elf::parse(&img).unwrap();
        assert_eq!(elf.symbol_table().err(), Some(ElfError::Truncated));
    }

    #[test]
    fn stripped_image_reports_no_symbols() {
        let mut img = build_elf(&[]);
        let shoff = u64::from_le_bytes(img[0x28..0x30].try_into().unwrap()) as usize;
        img[shoff + SHDR_SIZE + 4..shoff + SHDR_SIZE + 8].copy_from_slice(&0u32.to_le_bytes());
        let elf = Elf::parse(&img).unwrap();
        assert_eq!(elf.symbol_table().err(), Some(ElfError::NoSymbolTable));
    }
}
