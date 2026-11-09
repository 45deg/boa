// Run with --multishot-continuations. Control snapshots are copied; cells are shared.
console.log(reset(() => 1 + shift(k => k(2) + k(3)))); // 7

const withoutClosure = reset(() => {
    let count = 0;
    shift(k => k);
    return ++count;
});
const withClosure = reset(() => {
    let count = 0;
    globalThis.readCount = () => count;
    shift(k => k);
    return ++count;
});
console.log(withoutClosure(), withoutClosure(), withoutClosure()); // 1 2 3
console.log(withClosure(), withClosure(), withClosure()); // 1 2 3

const first = reset(() => {
    function inner() { return shift(k => k) + shift(k => k); }
    return 10 + inner();
});
const a = first(2), b = first(5);
console.log(a(3), b(4), a(9)); // 15 19 21

const throwing = reset(() => {
    const x = shift(k => k);
    if (x < 0) throw new Error('negative');
    return x * 2;
});
try { throwing(-1); } catch (e) { console.log(e.message); } // negative
console.log(throwing(3), throwing(4)); // 6 8
