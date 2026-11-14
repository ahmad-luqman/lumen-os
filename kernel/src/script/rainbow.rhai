// Colour bars, boxes and text: the drawing functions in one picture.
let w = screen_width();
let h = screen_height();
clear(0x101010);
let colours = [0xe6261f, 0xeb7532, 0xf7d038, 0xa3e048, 0x49da9a, 0x34bbe6, 0x4355db, 0xd23be7];
let bar = w / colours.len();
for i in 0..colours.len() {
    draw_rect(i * bar, 0, bar, 120, colours[i]);
}
// Boxes inside boxes.
for i in 0..8 {
    draw_rect(40 + i * 6, 160 + i * 6, 200 - i * 12, 200 - i * 12, colours[(i + 3) % 8]);
}
// A checkerboard of pixels.
for y in 0..64 {
    for x in 0..64 {
        if (x / 8 + y / 8) % 2 == 0 {
            draw_pixel(300 + x, 160 + y, 0xffffff);
        }
    }
}
draw_text(300, 240, "hello from rhai", 0xffd479, 0x101010);
draw_text(300, 264, `${cpus().len()} CPUs`, 0xe6e6e6, 0x101010);
