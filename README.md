# Merge tokscale exports

This small Rust tool merges `tokscale` export files from separate machines and opens a contribution-graph TUI backed by `tokscale-core`.

```sh
# Merge tokscale-export-*.json in the current directory and open the TUI
cargo run --release

# Pick files explicitly
cargo run --release -- machine-a.json machine-b.json -o tokscale-merged.json

# Give each export a real machine label for the Machines analysis tab
cargo run --release -- \
  --machine laptop=machine-a.json \
  --machine desktop=machine-b.json

# Only produce JSON
cargo run --release -- --no-tui
```

The merge is additive: it assumes each input came from a different machine and does not attempt cross-machine deduplication. Models are grouped by tokscale-core's canonical model id; clients, providers, agents, token buckets, costs, sessions, messages, and turns are summed. Monthly data is rebuilt from the merged daily data. The output also includes a `machines` array with one aggregate per input export.

The TUI uses `tokscale-core`'s `generate_graph_result` for daily summaries and intensity levels, then renders a terminal contribution graph with `ratatui`. The main page shows the last 30 days; `1`/`2`/`3`/`4` or Tab/arrow keys switch between Overview, Daily, Monthly, and Machines tabs. Daily and Monthly include spend and token sparklines, totals, token-bucket breakdowns, and model-spend breakdowns. Machines shows per-source spend, tokens, active days, models, messages, and source filenames. Without `--machine`, the input filename stem is used as the machine label. Press `q` or `Esc` to exit.

The dependency is pinned to Tokscale v4.5.3, currently the latest upstream tag, with current compatible crates locked by Cargo. Tokscale v4.5.3 has a JSON export writer and an internal TUI cache, but no supported command that imports an export JSON into the TUI. The cache schema is internal to `tokscale-cli`, so this tool consumes the stable export shape and uses the core graph API rather than fabricating cache files.
