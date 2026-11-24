use linux_raw_sys::general::*;

pub(crate) fn process(byte: u8, flags: u32, utf8: bool, column: &mut usize) -> ([u8; 2], usize) {
    if flags & OPOST == 0 {
        return ([byte, 0], 1);
    }
    match byte {
        b'\n' => {
            if flags & (ONLRET | ONLCR) != 0 {
                *column = 0;
            }
            if flags & ONLCR != 0 {
                return (*b"\r\n", 2);
            }
        }
        b'\r' => {
            if flags & ONOCR != 0 && *column == 0 {
                return ([0; 2], 0);
            }
            if flags & OCRNL != 0 {
                if flags & ONLRET != 0 {
                    *column = 0;
                }
                return ([b'\n', 0], 1);
            }
            *column = 0;
        }
        b'\t' => *column = column.saturating_add(8 - *column % 8),
        8 => *column = column.saturating_sub(1),
        32..=255 if byte != 127 && !(utf8 && byte & 0xc0 == 0x80) => {
            *column = column.saturating_add(1);
        }
        _ => {}
    }
    ([byte, 0], 1)
}
