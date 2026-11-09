/* Semantic regression tests for one-shot shift/reset: composition, lifetime,
 * closure mutation, unwinding, nested capture and unsupported call boundaries.
 * Run with cargo test -p boa_engine --lib vm::delimited; gc() is supplied by
 * the Rust harness to probe suspended-frame ownership.
 */
// Adapted from shift-reset-quickjs/tests/test_continuations.js (local MIT fork).
// Uses Boa host gc(), checks error classes rather than engine-specific messages.

let checks = 0;
function equal(actual, expected) {
    checks++;
    if (actual !== expected)
        throw new Error(`expected ${expected}, got ${actual}`);
}
function throws(body, fragment) {
    checks++;
    try { body(); } catch (error) {
        if (error instanceof TypeError) return;
        throw error;
    }
    throw new Error(`expected TypeError containing ${fragment}`);
}

equal(reset(() => 42), 42);
equal(reset(() => 1 + shift(k => 10)), 10);
equal(reset(() => 1 + shift(k => k(5) * 2)), 12);
function leaf() { return 3 + shift(k => k(4)); }
function middle() { return 2 * leaf(); }
equal(reset(() => 1 + middle()), 15);
equal(reset(() => 1 + reset(() => 2 + shift(k => k(3)))), 6);
equal(reset(() => 1 + shift(k => 2 + shift(j => j(7)))), 9);

let saved;
equal(reset(() => 3 + shift(k => { saved = k; return 'paused'; })), 'paused');
gc();
equal(saved(7), 10);
throws(() => saved(8), 'consumed');

let read, write;
saved = reset(() => {
    let x = 1;
    read = () => x;
    write = v => { x = v; };
    const y = shift(k => k);
    x += y;
    return x;
});
write(20);
gc();
equal(saved(2), 22);
equal(read(), 22);

let log = '';
saved = reset(() => {
    try { log += 'a'; return 2 + shift(k => k); }
    finally { log += 'f'; }
});
equal(log, 'a');
equal(saved(5), 7);
equal(log, 'af');

let token = {};
saved = reset(() => {
    try {
        function inner() { shift(k => k); throw token; }
        inner();
    } catch (error) { return error === token; }
});
equal(saved(), true);

// A frame that catches a resumed exception may suspend and resume again.
saved = reset(() => {
    try {
        function fail() { shift(k => k); throw token; }
        fail();
    } catch (error) { return 4 + shift(k => k); }
});
const recovered = saved();
equal(recovered(6), 10);

// A second shift after resumption includes the remaining outer frames.
saved = reset(() => {
    function inner() { return shift(k => k) + shift(k => k); }
    return 10 + inner();
});
const second = saved(2);
gc();
equal(second(3), 15);
throws(() => saved(), 'consumed');

const receiver = { n: 8, run(x) { return this.n + x + shift(k => k); } };
saved = reset(() => receiver.run(2));
equal(saved(3), 13);

saved = reset(() => shift(k => k));
equal(saved(), undefined);
throws(() => saved(), 'consumed');

let reentrant;
reentrant = reset(() => { shift(k => k); return reentrant(); });
throws(() => reentrant(), 'consumed');
throws(() => reentrant(), 'consumed');

throws(() => shift(k => k), 'active reset');
throws(() => reset(1), 'callable');
throws(() => reset(() => shift(1)), 'callable');
throws(() => reset(() => [1].map(() => shift(k => k))), 'native');
throws(() => reset(() => ({ get x() { return shift(k => k); } }).x), 'boundary');
throws(() => reset(() => { function C() { shift(k => k); } return new C(); }), 'boundary');
throws(() => reset(() => (function* () { yield shift(k => k); })().next()), 'boundary');
throws(() => reset(() => shift.call(null, k => k)), 'native');

// A fresh reset inside a native callback establishes a usable boundary.
equal([1].map(x => reset(() => x + shift(k => k(2))))[0], 3);
equal(reset(() => {
    const k = reset(() => 2 + shift(k => k));
    return 10 + k(3);
}), 15);
let effects = 0;
saved = reset(() => { effects++; return shift(k => k) + effects; });
equal(saved(2), 3);
equal(effects, 1);

// The resumed continuation's fresh reset separates it from its handler.
equal(reset(() => {
    const action = shift(k => 100 + k(() => shift(j => 5)));
    action();
    return 20;
}), 105);

// Non-strict arguments aliasing must survive suspension.
saved = reset(Function('x', 'return function(a) { const args = arguments; '
    + 'const v = shift(k => k); args[0] += v; return a; }(10);'));
equal(saved(4), 14);
equal(reset(() => { const f = () => shift(k => k); return f(...[]); })(8), 8);
throws(() => reset(() => eval('shift(k => k)')), 'boundary');
throws(() => reset(() => new Proxy(() => shift(k => k), {})()), 'native');

// Suspension in a loop preserves the loop state and the accumulator.
saved = reset(() => {
    let total = 0;
    for (let i = 0; i < 3; i++) total += shift(k => k);
    return total;
});
equal(saved(1)(2)(3), 6);

// Abandoned continuations do not execute finally, including during collection.
log = '';
reset(() => { try { shift(k => 0); } finally { log += 'f'; } });
gc();
equal(log, '');
for (let i = 0; i < 1000; i++) {
    let cyclic;
    reset(() => { cyclic = () => cyclic; shift(k => { cyclic = k; }); });
}
gc();
checks;
