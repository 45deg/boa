// Run with: cargo run -p boa_cli --no-default-features -- --delimited-continuations examples/continuations.js
// Demonstrates one-shot composition, a shift in the handler, and recapture.
console.log(reset(() => 1 + shift(k => k(2) * 10))); // 30
console.log(reset(() => 1 + shift(k => 2 + shift(j => j(7))))); // 9
const first = reset(() => {
    function inner() { return shift(k => k) + shift(k => k); }
    return 10 + inner();
});
const second = first(2);
console.log(second(3)); // 15
try { first(4); } catch (e) { console.log(e instanceof TypeError); } // true
try {
    reset(() => [1].map(() => shift(k => k)));
} catch (e) {
    console.log(e instanceof TypeError); // true: capture crosses a native callback
}
console.log([1].map(x => reset(() => x + shift(k => k(2))))[0]); // 3
