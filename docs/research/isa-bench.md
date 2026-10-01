<!-- Generated: cargo run --release -p nanox-ir --example bench (stdout table; AMD Ryzen 7 5800H, WSL2, rustc 1.90, release profile of this workspace). Best of 3 timings of 4-40 million evaluations; run-to-run noise about 10-15 percent. -->

| policy | instructions | steps per run | native Rust ns | rule table ns | NIR A ns | NIR B ns | NIR B optimized ns | B opt / native |
|---|---|---|---|---|---|---|---|---|
| service admission | 33 to 26 | 17.6 to 14.6 | 1.4 | n/a | 49.2 | 54.2 | 46.3 | 34x |
| frame filter | 44 to 44 | 16.6 to 16.6 | 1.5 | 14.4 | 47.7 | 50.6 | 50.2 | 34x |
| score (arithmetic) | 29 to 29 | 29.0 to 29.0 | 1.8 | n/a | 108.1 | 589.7 | 589.0 | 327x |
| placement (select) | 19 to 18 | 19.0 to 18.0 | 1.1 | n/a | 76.1 | 75.3 | 73.2 | 69x |

Result distributions over the 4096 inputs (value:count): admission 0:2978 1:480 2:638; filter 0:2568 1:1528; score spread over 0..14; placement 0:1050 1:972 2:1037 3:1037.
