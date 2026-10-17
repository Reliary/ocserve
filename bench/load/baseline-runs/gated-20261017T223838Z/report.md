# Load report — ocserve vs upstream (k6 L1, fixture arms)

- fixture: 201 sessions (msgs=48505, parts=178141), deep=ses_056d974baffek0EtrIOJ…, lever=3
- cpu pinning (operator directive): class=homogeneous · ocserve=[0,4] freeze=[1,5] k6=[2,6] — each arm owns whole physical cores (both SMT threads), k6 on separate cores
- run order (rounds interleaved): r1:ocserve-freeze
- ramp targets (concurrency ladder): 50,75,150
- **claim classes**: achieved-capacity + error-rate valid at n=1; arm-to-arm latency deltas are INDICATIVE until ROUNDS>=3 (2.7× rule)

## Results (per arm × mode × scenario script)

| arm | mode | script | p50 ms | p95 ms | p99 ms | reqs | failed | req/s | peak RSS MB | cpu s |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| freeze | arrival | arrival | 0.0 | 778.686 | 8197.44 | 150105 | 95.171% | 2501.515 | 3088.7 | 526.6 |
| freeze | hot | read-hot | 154.252 | 561.878 | 751.587 | 85592 | 0.0% | 417.514 | 3088.7 | 526.6 |
| freeze | spread | read-hot | 175.972 | 576.962 | 824.663 | 77395 | 0.0% | 377.535 | 3088.7 | 526.6 |
| ocserve | arrival | arrival | 138.531 | 536.242 | 787.866 | 190799 | 0.0% | 3157.015 | 724.2 | 1469.6 |
| ocserve | hot | read-hot | 9.1 | 34.643 | 56.626 | 1380989 | 0.0% | 6736.508 | 724.2 | 1469.6 |
| ocserve | spread | read-hot | 8.243 | 40.921 | 75.28 | 1299488 | 0.0% | 6338.948 | 724.2 | 1469.6 |

- loadavg1 during run: min 0.08 / max 15.57 (quiet-host gate applies at start; recorded for latency context)
- MemAvailable during run: min 0.1 GB (co-tenant guard refuses runs below 1.5 GB at start)

## Per-endpoint p95 (informational — pooled p95 is the gate)

| export | session_list | message_page | message_cursor | config | agent | command | file_list | search | session_status |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| r1-freeze-arrival-arrival | 3763.589 | 7353.973 | 7508.561 | 3193.222 | 693.7 | 1568.77 | 0.0 | 0.0 | 0.0 |
| r1-freeze-hot-readhot | 610.761 | 627.887 | 634.966 | 526.978 | 419.588 | 422.363 | 762.572 | 418.241 | 395.167 |
| r1-freeze-spread-readhot | 561.022 | 598.626 | 545.399 | 533.607 | 491.529 | 443.3 | 931.885 | 527.241 | 375.409 |
| r1-ocserve-arrival-arrival | 501.09 | 745.297 | 790.865 | 293.877 | 317.36 | 306.138 | 512.358 | 482.428 | 299.909 |
| r1-ocserve-hot-readhot | 32.076 | 34.955 | 35.841 | 30.933 | 31.04 | 30.452 | 39.758 | 44.378 | 30.03 |
| r1-ocserve-spread-readhot | 36.396 | 58.824 | 61.329 | 29.514 | 28.384 | 28.554 | 46.355 | 50.243 | 31.001 |

## Sampler peaks

- ocserve: peak RSS 724.2 MB, cpu 1469.6 s over sampled window
- freeze: peak RSS 3088.7 MB, cpu 526.6 s over sampled window
- ocserve end-state gauges: ocserve_prompt_locks=0, ocserve_rss_bytes=9.57235e+06, ocserve_writer_queue_depth=0

