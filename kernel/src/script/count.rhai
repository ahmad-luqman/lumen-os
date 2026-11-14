// Count up twice a second until Ctrl-C.
let n = 0;
loop {
    print(n);
    n += 1;
    sleep(500);
}
