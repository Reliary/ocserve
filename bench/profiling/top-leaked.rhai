// Top leaked (still-live-at-exit) groups by TOTAL BYTES, with stacks.
// Complements top-alloc.rhai (which sorts by count); this tells us what the
// settled live set is made of for K-OOM/size work.
let all = allocations();
let lg = all.only_leaked().group_by_backtrace().sort_by_size();
println("live allocations: {}", all.only_leaked().len());
let n = lg.len();
if n > 14 {
    n = 14;
}
for i in 0..n {
    let g = lg[i];
    let mut total = 0;
    let mut k = 0;
    while k < g.len() {
        total += g[k].size();
        k += 1;
    }
    println("--- #{} count={} bytes={:.1}MB ---", i, g.len(), total / 1048576.0);
    println(g[0].backtrace().strip());
    println();
}
