//! An app's manifest: a custom section named `lumen.manifest`, exactly
//! `caps=` followed by zero or more [`Listable`] grants in lower case,
//! separated by single commas (`caps=time,desktop`, or `caps=`), with no
//! whitespace or newline. [`find`] walks the module's sections itself (no
//! wasm parser: the kernel reads the manifest before wasmi validates the
//! module) and refuses, naming why: no manifest, two sections, an unknown or
//! repeated grant, anything off the grammar, or bytes that aren't a module's
//! sections. It never panics, whatever the bytes (tested on every
//! truncation and flipped byte of a module).

use crate::{Grants, Listable};
use alloc::string::String;
use core::fmt;

/// The section's name.
pub const SECTION: &str = "lumen.manifest";

/// What a manifest says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The listable grants it asks for (never `Log`: every app has that).
    pub caps: Grants,
}

/// Why [`find`] refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// Not a wasm module (no `\0asm` version 1 header).
    NotAModule,
    /// A section's length or name runs past the module's end, or a length
    /// isn't a valid LEB128 u32.
    Malformed,
    /// No `lumen.manifest` section.
    Missing,
    /// More than one `lumen.manifest` section.
    Twice,
    /// The section doesn't start with `caps=`, has an empty grant (two
    /// commas, or one at the end) or isn't UTF-8.
    Grammar,
    /// A grant no app can list (`log` included: every app has it).
    Unknown(String),
    /// A grant listed twice.
    Repeated(Listable),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::NotAModule => f.write_str("not a wasm module"),
            ManifestError::Malformed => f.write_str("the module's sections run past its end"),
            ManifestError::Missing => write!(f, "no {SECTION} section"),
            ManifestError::Twice => write!(f, "two {SECTION} sections"),
            ManifestError::Grammar => write!(f, "the manifest isn't `caps=` and a comma-separated list"),
            ManifestError::Unknown(name) => write!(f, "the manifest names an unknown grant `{name}`"),
            ManifestError::Repeated(g) => write!(f, "the manifest names `{}` twice", g.name()),
        }
    }
}

/// The module's manifest, read from its custom sections.
pub fn find(module: &[u8]) -> Result<Manifest, ManifestError> {
    let mut sections = Sections::new(module).ok_or(ManifestError::NotAModule)?;
    let mut found = None;
    while let Some(section) = sections.next() {
        let (id, body) = section.ok_or(ManifestError::Malformed)?;
        if id != 0 {
            continue;
        }
        let (name, payload) = custom(body).ok_or(ManifestError::Malformed)?;
        if name != SECTION.as_bytes() {
            continue;
        }
        if found.replace(payload).is_some() {
            return Err(ManifestError::Twice);
        }
    }
    parse(found.ok_or(ManifestError::Missing)?)
}

/// A manifest section's payload, parsed.
pub fn parse(payload: &[u8]) -> Result<Manifest, ManifestError> {
    let text = core::str::from_utf8(payload).map_err(|_| ManifestError::Grammar)?;
    let list = text.strip_prefix("caps=").ok_or(ManifestError::Grammar)?;
    let mut caps = Grants::NONE;
    if list.is_empty() {
        return Ok(Manifest { caps });
    }
    for name in list.split(',') {
        if name.is_empty() {
            return Err(ManifestError::Grammar);
        }
        let l = Listable::from_name(name).ok_or_else(|| ManifestError::Unknown(String::from(name)))?;
        if caps.has(l.grant()) {
            return Err(ManifestError::Repeated(l));
        }
        caps = caps.with(l.grant());
    }
    Ok(Manifest { caps })
}

/// A wasm module's sections, as (id, body), or `None` once one runs past
/// the end.
struct Sections<'a> {
    rest: &'a [u8],
}

impl<'a> Sections<'a> {
    fn new(module: &'a [u8]) -> Option<Sections<'a>> {
        let rest = module.strip_prefix(b"\0asm\x01\0\0\0")?;
        Some(Sections { rest })
    }

    /// The next section: `None` at the end, `Some(None)` if it is
    /// malformed (and nothing after it is read).
    fn next(&mut self) -> Option<Option<(u8, &'a [u8])>> {
        let (&id, rest) = self.rest.split_first()?;
        let parsed = leb128_u32(rest).and_then(|(len, rest)| {
            let len = usize::try_from(len).ok()?;
            (len <= rest.len()).then(|| rest.split_at(len))
        });
        Some(match parsed {
            Some((body, rest)) => {
                self.rest = rest;
                Some((id, body))
            }
            None => {
                self.rest = &[];
                None
            }
        })
    }
}

/// A custom section's (name, payload).
fn custom(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = leb128_u32(body)?;
    let len = usize::try_from(len).ok()?;
    (len <= rest.len()).then(|| rest.split_at(len))
}

/// An unsigned LEB128 u32 (at most 5 bytes, no bits past 32) and what
/// follows it.
fn leb128_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let mut value: u32 = 0;
    for (i, &b) in bytes.iter().enumerate().take(5) {
        let low = u32::from(b & 0x7f);
        if i == 4 && low > 0x0f {
            return None;
        }
        value |= low << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, &bytes[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Grant;
    use alloc::vec::Vec;

    fn leb(mut n: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn section(id: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(leb(body.len() as u32));
        out.extend(body);
        out
    }

    fn custom_section(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut body = leb(name.len() as u32);
        body.extend(name.as_bytes());
        body.extend(payload);
        section(0, &body)
    }

    /// A module: a type section, then the given custom sections, then a
    /// function section.
    fn module(customs: &[(&str, &[u8])]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        m.extend(section(1, &[0x01, 0x60, 0x00, 0x01, 0x7f]));
        for (name, payload) in customs {
            m.extend(custom_section(name, payload));
        }
        m.extend(section(3, &[0x01, 0x00]));
        m
    }

    #[test]
    fn a_manifest_names_its_grants() {
        let m = find(&module(&[("name", b"x"), (SECTION, b"caps=time,trace")])).unwrap();
        assert_eq!(m.caps, Grants::NONE.with(Grant::Time).with(Grant::Trace));
        assert_eq!(find(&module(&[(SECTION, b"caps=")])).unwrap().caps, Grants::NONE);
        let all = find(&module(&[(SECTION, b"caps=desktop,time,trace")])).unwrap();
        assert_eq!(all.caps.iter().count(), 3);
    }

    #[test]
    fn each_rule_has_its_refusal() {
        let cases: [(&[u8], ManifestError); 11] = [
            (b"caps=time,", ManifestError::Grammar),
            (b"caps=,time", ManifestError::Grammar),
            (b"caps=time,,trace", ManifestError::Grammar),
            (b"cap=time", ManifestError::Grammar),
            (b"caps=time\n", ManifestError::Unknown("time\n".into())),
            (b" caps=time", ManifestError::Grammar),
            (b"caps=log", ManifestError::Unknown("log".into())),
            (b"caps=Time", ManifestError::Unknown("Time".into())),
            (b"caps=time, trace", ManifestError::Unknown(" trace".into())),
            (b"caps=time,time", ManifestError::Repeated(Listable::Time)),
            (b"caps=\xff", ManifestError::Grammar),
        ];
        for (payload, want) in cases {
            assert_eq!(find(&module(&[(SECTION, payload)])), Err(want), "{payload:?}");
        }
        assert_eq!(find(&module(&[("other", b"caps=")])), Err(ManifestError::Missing));
        assert_eq!(find(&module(&[(SECTION, b"caps="), (SECTION, b"caps=")])), Err(ManifestError::Twice));
        assert_eq!(find(b"\0asm\x02\0\0\0"), Err(ManifestError::NotAModule));
        assert_eq!(find(b""), Err(ManifestError::NotAModule));
        // A section claiming more bytes than there are.
        let mut m = module(&[]);
        m.extend([0x00, 0x10, 0x01]);
        assert_eq!(find(&m), Err(ManifestError::Malformed));
        // A length past 32 bits, and one never ending.
        let mut m = module(&[]);
        m.extend([0x00, 0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert_eq!(find(&m), Err(ManifestError::Malformed));
        let mut m = module(&[]);
        m.extend([0x00, 0x80, 0x80]);
        assert_eq!(find(&m), Err(ManifestError::Malformed));
        // A custom section whose name runs past it.
        let mut m = module(&[]);
        m.extend(section(0, &[0x20, b'l']));
        assert_eq!(find(&m), Err(ManifestError::Malformed));
    }

    #[test]
    fn leb128_reads_u32s_and_refuses_the_rest() {
        assert_eq!(leb128_u32(&[0x00]), Some((0, &[][..])));
        assert_eq!(leb128_u32(&[0xe5, 0x8e, 0x26, 9]), Some((624_485, &[9][..])));
        assert_eq!(leb128_u32(&[0xff, 0xff, 0xff, 0xff, 0x0f]), Some((u32::MAX, &[][..])));
        assert_eq!(leb128_u32(&[0xff, 0xff, 0xff, 0xff, 0x1f]), None);
        assert_eq!(leb128_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]), None);
        assert_eq!(leb128_u32(&[0x80]), None);
    }

    #[test]
    fn no_truncation_or_flipped_byte_panics() {
        let m = module(&[("name", b"\x00\x05hello"), (SECTION, b"caps=time,desktop"), ("producers", b"\x01x")]);
        assert!(find(&m).is_ok());
        for cut in 0..m.len() {
            let _ = find(&m[..cut]);
        }
        for i in 0..m.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut bad = m.clone();
                bad[i] ^= flip;
                let _ = find(&bad);
            }
        }
    }
}
