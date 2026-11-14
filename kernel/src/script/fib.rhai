// The first Fibonacci numbers, each worked out recursively.
fn fib(n) {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}
for n in 0..11 {
    print(`fib(${n}) = ${fib(n)}`);
}
