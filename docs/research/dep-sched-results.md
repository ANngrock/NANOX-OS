<!-- Generated: cargo run --release -p dep-sched --example report -- --root . ; costs from docs/research/dep-sched-costs.tsv. Do not edit by hand. -->

## Dependency scheduling against the declaration-order baseline

### Synthetic graphs (48 tasks, 1000 instances per row)

| shape | costs | labels | workers | FIFO/LB | CP/LB | CP better | equal | worse | mean gain | worst loss |
|---|---|---|---|---|---|---|---|---|---|---|
| layered | uniform 1-10 | declared | 2 | 1.081 | 1.012 | 97.6% | 2.1% | 0.3% | 6.3% | 1.5% |
| layered | uniform 1-10 | declared | 4 | 1.052 | 1.005 | 87.6% | 12.2% | 0.2% | 4.3% | 2.1% |
| layered | uniform 1-10 | declared | 8 | 1.000 | 1.000 | 0.1% | 99.9% | 0.0% | 0.0% | 0.0% |
| layered | uniform 1-10 | shuffled | 2 | 1.091 | 1.014 | 97.8% | 2.0% | 0.2% | 6.9% | 0.8% |
| layered | uniform 1-10 | shuffled | 4 | 1.058 | 1.006 | 86.6% | 13.3% | 0.1% | 4.7% | 2.2% |
| layered | uniform 1-10 | shuffled | 8 | 1.000 | 1.000 | 0.5% | 99.5% | 0.0% | 0.0% | 0.0% |
| layered | bimodal | declared | 2 | 1.130 | 1.036 | 98.2% | 1.1% | 0.7% | 8.1% | 2.9% |
| layered | bimodal | declared | 4 | 1.026 | 1.004 | 76.7% | 23.1% | 0.2% | 2.0% | 2.4% |
| layered | bimodal | declared | 8 | 1.000 | 1.000 | 0.7% | 99.3% | 0.0% | 0.0% | 0.0% |
| layered | bimodal | shuffled | 2 | 1.135 | 1.035 | 98.6% | 1.1% | 0.3% | 8.5% | 2.1% |
| layered | bimodal | shuffled | 4 | 1.023 | 1.003 | 69.8% | 30.0% | 0.2% | 1.9% | 0.7% |
| layered | bimodal | shuffled | 8 | 1.000 | 1.000 | 0.3% | 99.7% | 0.0% | 0.0% | 0.0% |
| random | uniform 1-10 | declared | 2 | 1.014 | 1.001 | 73.8% | 24.7% | 1.5% | 1.2% | 0.9% |
| random | uniform 1-10 | declared | 4 | 1.065 | 1.005 | 94.8% | 4.6% | 0.6% | 5.5% | 1.7% |
| random | uniform 1-10 | declared | 8 | 1.056 | 1.002 | 54.8% | 44.9% | 0.3% | 4.6% | 2.8% |
| random | uniform 1-10 | shuffled | 2 | 1.030 | 1.001 | 88.6% | 11.2% | 0.2% | 2.8% | 0.8% |
| random | uniform 1-10 | shuffled | 4 | 1.135 | 1.005 | 99.5% | 0.4% | 0.1% | 11.0% | 1.4% |
| random | uniform 1-10 | shuffled | 8 | 1.130 | 1.002 | 85.6% | 14.3% | 0.1% | 10.6% | 1.8% |
| random | bimodal | declared | 2 | 1.042 | 1.000 | 85.3% | 14.2% | 0.5% | 3.8% | 2.2% |
| random | bimodal | declared | 4 | 1.131 | 1.008 | 92.9% | 6.7% | 0.4% | 10.2% | 6.5% |
| random | bimodal | declared | 8 | 1.014 | 1.000 | 33.4% | 66.6% | 0.0% | 1.3% | 0.0% |
| random | bimodal | shuffled | 2 | 1.063 | 1.000 | 91.6% | 8.1% | 0.3% | 5.6% | 0.6% |
| random | bimodal | shuffled | 4 | 1.174 | 1.010 | 97.1% | 2.5% | 0.4% | 13.2% | 4.1% |
| random | bimodal | shuffled | 8 | 1.025 | 1.000 | 54.9% | 45.0% | 0.1% | 2.3% | 4.2% |
| fork-join | uniform 1-10 | declared | 2 | 1.053 | 1.044 | 65.2% | 27.0% | 7.8% | 0.8% | 2.3% |
| fork-join | uniform 1-10 | declared | 4 | 1.092 | 1.073 | 53.9% | 43.2% | 2.9% | 1.7% | 2.9% |
| fork-join | uniform 1-10 | declared | 8 | 1.000 | 1.000 | 0.0% | 100.0% | 0.0% | 0.0% | 0.0% |
| fork-join | uniform 1-10 | shuffled | 2 | 1.116 | 1.044 | 91.4% | 6.3% | 2.3% | 6.1% | 1.5% |
| fork-join | uniform 1-10 | shuffled | 4 | 1.205 | 1.076 | 67.0% | 33.0% | 0.0% | 9.5% | 0.0% |
| fork-join | uniform 1-10 | shuffled | 8 | 1.000 | 1.000 | 0.0% | 100.0% | 0.0% | 0.0% | 0.0% |
| fork-join | bimodal | declared | 2 | 1.080 | 1.043 | 80.7% | 13.7% | 5.6% | 3.4% | 7.3% |
| fork-join | bimodal | declared | 4 | 1.055 | 1.015 | 53.0% | 46.1% | 0.9% | 3.5% | 8.4% |
| fork-join | bimodal | declared | 8 | 1.000 | 1.000 | 0.0% | 100.0% | 0.0% | 0.0% | 0.0% |
| fork-join | bimodal | shuffled | 2 | 1.130 | 1.043 | 92.4% | 5.1% | 2.5% | 7.4% | 4.4% |
| fork-join | bimodal | shuffled | 4 | 1.090 | 1.015 | 46.4% | 53.3% | 0.3% | 5.9% | 3.9% |
| fork-join | bimodal | shuffled | 8 | 1.000 | 1.000 | 0.0% | 100.0% | 0.0% | 0.0% | 0.0% |
| out-tree | uniform 1-10 | declared | 2 | 1.033 | 1.022 | 72.5% | 27.0% | 0.5% | 1.1% | 0.8% |
| out-tree | uniform 1-10 | declared | 4 | 1.118 | 1.074 | 95.2% | 4.7% | 0.1% | 3.9% | 1.4% |
| out-tree | uniform 1-10 | declared | 8 | 1.062 | 1.017 | 58.7% | 41.2% | 0.1% | 3.9% | 4.3% |
| out-tree | uniform 1-10 | shuffled | 2 | 1.044 | 1.022 | 84.8% | 15.0% | 0.2% | 2.1% | 0.8% |
| out-tree | uniform 1-10 | shuffled | 4 | 1.171 | 1.074 | 98.4% | 1.5% | 0.1% | 8.0% | 1.4% |
| out-tree | uniform 1-10 | shuffled | 8 | 1.122 | 1.017 | 81.6% | 18.4% | 0.0% | 8.7% | 0.0% |
| out-tree | bimodal | declared | 2 | 1.059 | 1.022 | 84.0% | 16.0% | 0.0% | 3.4% | 0.0% |
| out-tree | bimodal | declared | 4 | 1.152 | 1.040 | 93.2% | 6.5% | 0.3% | 9.3% | 2.8% |
| out-tree | bimodal | declared | 8 | 1.024 | 1.003 | 28.9% | 70.9% | 0.2% | 1.7% | 10.9% |
| out-tree | bimodal | shuffled | 2 | 1.074 | 1.021 | 90.3% | 9.7% | 0.0% | 4.7% | 0.0% |
| out-tree | bimodal | shuffled | 4 | 1.198 | 1.040 | 97.9% | 2.1% | 0.0% | 12.6% | 0.0% |
| out-tree | bimodal | shuffled | 8 | 1.035 | 1.003 | 45.7% | 54.2% | 0.1% | 2.7% | 1.3% |
| in-tree | uniform 1-10 | declared | 2 | 1.051 | 1.026 | 80.2% | 13.6% | 6.2% | 2.3% | 3.1% |
| in-tree | uniform 1-10 | declared | 4 | 1.209 | 1.086 | 98.4% | 1.4% | 0.2% | 9.8% | 2.5% |
| in-tree | uniform 1-10 | declared | 8 | 1.197 | 1.022 | 94.0% | 6.0% | 0.0% | 14.0% | 0.0% |
| in-tree | uniform 1-10 | shuffled | 2 | 1.075 | 1.026 | 91.5% | 6.2% | 2.3% | 4.5% | 1.5% |
| in-tree | uniform 1-10 | shuffled | 4 | 1.270 | 1.086 | 99.6% | 0.2% | 0.2% | 14.0% | 1.4% |
| in-tree | uniform 1-10 | shuffled | 8 | 1.271 | 1.024 | 98.3% | 1.7% | 0.0% | 18.8% | 0.0% |
| in-tree | bimodal | declared | 2 | 1.080 | 1.023 | 90.2% | 7.4% | 2.4% | 5.1% | 6.2% |
| in-tree | bimodal | declared | 4 | 1.235 | 1.049 | 98.5% | 0.8% | 0.7% | 14.4% | 4.5% |
| in-tree | bimodal | declared | 8 | 1.070 | 1.003 | 77.3% | 22.7% | 0.0% | 5.8% | 0.0% |
| in-tree | bimodal | shuffled | 2 | 1.103 | 1.025 | 95.3% | 3.6% | 1.1% | 6.8% | 3.3% |
| in-tree | bimodal | shuffled | 4 | 1.276 | 1.052 | 99.1% | 0.7% | 0.2% | 16.9% | 0.8% |
| in-tree | bimodal | shuffled | 8 | 1.088 | 1.004 | 82.0% | 17.9% | 0.1% | 7.2% | 1.6% |

All 60000 instances: FIFO/LB 1.089, CP/LB 1.021; critical path better on 70.9%, equal on 28.4%, worse on 0.7% (worst loss 10.9%); mean gain 5.6%.

### This workspace (22 crates)

Task graph: the workspace members in declaration order, path dependencies (including dev-dependencies) as edges: 11 edges. Cost: measured wall-clock milliseconds to compile the crate alone (docs/research/dep-sched-costs.tsv).

Source lines against measured compile time over 22 crates: correlation r = 0.38.

| workers | FIFO (declared) | FIFO (shuffled, mean of 200) | critical path | lower bound |
|---|---|---|---|---|
| 1 | 6601 | 6601 | 6601 | 6601 |
| 2 | 3683 | 3486 | 3310 | 3301 |
| 4 | 2226 | 2008 | 1700 | 1651 |
| 8 | 1636 | 1389 | 1186 | 1186 |
| 16 | 1186 | 1186 | 1186 | 1186 |

