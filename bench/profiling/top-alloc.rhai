let all = allocations();
println("total allocations: {}", all.len());
println("leaked at exit:    {}", all.only_leaked().len());
println("temporary (churn): {}", all.only_temporary().len());
println();

let groups = all.group_by_backtrace().sort_by_size();
let n = groups.len();
if n > 12 {
    n = 12;
}
println("=== top {} allocation groups by size (desc) ===", n);
for i in 0..n {
    let g = groups[i];
    println("--- #{} count={} ---", i, g.len());
    println(g[0].backtrace().strip());
    println();
}

println();
println("=== LEAKED-AT-EXIT groups (top 5 by size) ===");
let lg = all.only_leaked().group_by_backtrace().sort_by_size();
let ln = lg.len();
if ln > 5 {
    ln = 5;
}
for i in 0..ln {
    let g = lg[i];
    println("--- leaked #{} count={} ---", i, g.len());
    println(g[0].backtrace().strip());
    println();
}
