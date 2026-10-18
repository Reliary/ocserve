# Load report — ocserve vs upstream (k6 L1, fixture arms)

- fixture: 201 sessions (msgs=48505, parts=178141), deep=ses_056d974baffek0EtrIOJ…, lever=3
- cpu pinning (operator directive): class=homogeneous · ocserve=[0,4] freeze=[1,5] k6=[2,6] — each arm owns whole physical cores (both SMT threads), k6 on separate cores
- run order (rounds interleaved): r1:ocserve-freeze r2:freeze-ocserve
- ramp targets (concurrency ladder): 50,75,150
- **claim classes**: achieved-capacity + error-rate valid at n=1; arm-to-arm latency deltas are INDICATIVE until ROUNDS>=3 (2.7× rule)

## Results (per arm × mode × scenario script)

| arm | mode | script | p50 ms | p95 ms | p99 ms | reqs | failed | req/s | peak RSS MB | cpu s |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| freeze | arrival | arrival | 0.0 | 1138.794 | 8584.567 | 144521 | 93.923% | 2408.568 | 3151.3 | 539.0 |
| freeze | hot | read-hot | 144.934 | 553.522 | 802.626 | 85546 | 0.0% | 417.285 | 3151.3 | 539.0 |
| freeze | spread | read-hot | 173.816 | 581.365 | 904.808 | 76330 | 0.0% | 372.34 | 3151.3 | 539.0 |
| ocserve | arrival | arrival | 111.429 | 410.811 | 552.856 | 195645 | 0.0% | 3245.299 | 618.3 | 1704.4 |
| ocserve | hot | read-hot | 8.756 | 34.1 | 55.11 | 1425508 | 0.0% | 6953.662 | 618.3 | 1704.4 |
| ocserve | spread | read-hot | 9.735 | 43.052 | 77.951 | 1196025 | 0.0% | 5834.117 | 618.3 | 1704.4 |

- loadavg1 during run: min 1.1 / max 14.27 (quiet-host gate applies at start; recorded for latency context)
- MemAvailable during run: min 0.1 GB (co-tenant guard refuses runs below 1.5 GB at start)

## Per-endpoint p95 (informational — pooled p95 is the gate)

| export | session_list | message_page | message_cursor | config | agent | command | file_list | search | session_status |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| r1-freeze-arrival-arrival | 3546.48 | 7879.134 | 8074.867 | 4121.818 | 803.436 | 1220.62 | 1018.303 | 0.0 | 0.0 |
| r1-freeze-hot-readhot | 552.548 | 744.149 | 588.746 | 390.742 | 497.138 | 361.073 | 748.485 | 487.725 | 308.853 |
| r1-freeze-spread-readhot | 560.483 | 583.974 | 554.058 | 503.942 | 498.25 | 489.761 | 999.439 | 477.69 | 347.869 |
| r1-ocserve-arrival-arrival | 396.986 | 505.221 | 532.742 | 328.899 | 332.02 | 333.021 | 406.661 | 410.447 | 343.039 |
| r1-ocserve-hot-readhot | 33.072 | 36.291 | 36.352 | 31.716 | 31.121 | 30.681 | 38.194 | 37.123 | 31.073 |
| r1-ocserve-spread-readhot | 37.379 | 63.91 | 68.723 | 33.94 | 31.443 | 31.287 | 47.294 | 52.701 | 35.391 |

## Sampler peaks

- ocserve: peak RSS 618.3 MB, cpu 1704.4 s over sampled window
- freeze: peak RSS 3151.3 MB, cpu 539.0 s over sampled window
- ocserve end-state gauges: ocserve_prompt_locks=0, ocserve_rss_bytes=3.49225e+07, ocserve_writer_queue_depth=0

