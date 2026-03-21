# Rust Backtester

This repo is a self-contained Rust backtester for IMC Prosperity 4.

It only supports local backtesting. There is no API surface and no hosted workflow in this repo.

Everything needed for the default backtest flow now lives inside this directory. The bundled default trader is:

- `traders/latest_trader.py`

## Setup

Clone the repo:

```bash
git clone https://github.com/GeyzsoN/prosperity_rust_backtester.git
cd prosperity_rust_backtester
```

### macOS

Install the toolchain once:

```bash
xcode-select --install
curl https://sh.rustup.rs -sSf | sh
source "$HOME/.cargo/env"
python3 --version
```

Then either install the CLI:

```bash
make install
```

or just run the backtester directly:

```bash
make backtest
```

### Windows

Use WSL2. Open an Ubuntu shell inside WSL2 and run the same commands there. Native Windows shells are not the target environment for this repo.

There is no separate manual build step required for normal use:

- `make backtest` and the other `make` run targets use `cargo run`, which builds automatically on first use
- `make install` installs the CLI once so you can run `rust_backtester` directly afterward

## Included Data

The repo is organized by round:

- `datasets/tutorial/prices_round_0_day_-2.csv`
- `datasets/tutorial/trades_round_0_day_-2.csv`
- `datasets/tutorial/prices_round_0_day_-1.csv`
- `datasets/tutorial/trades_round_0_day_-1.csv`
- `datasets/tutorial/submission.log`
- `datasets/round1/`
- `datasets/round2/`
- `datasets/round3/`
- `datasets/round4/`
- `datasets/round5/`
- `datasets/round6/`
- `datasets/round7/`
- `datasets/round8/`

Right now the bundled public data is the raw IMC tutorial day data in `datasets/tutorial/`, plus a sample tutorial `submission.log` produced with the bundled basic trader. The other round folders are there so future round files can be placed in the correct folder instead of being mixed together.
If you place a portal `submission.log` file into a round folder, the backtester will use it. `submission.log` is also generated for persisted runs.

## CLI

The CLI is intentionally simple:

```bash
rust_backtester
```

With no arguments it will:

- auto-pick the newest Python file that looks like a trader from this repo's local `scripts/`, `traders/submissions/`, or `traders/`
- default the dataset to the latest populated round folder under `datasets/`
- run in fast mode
- print one compact row per day

That means the simplest local commands are:

```bash
make backtest
```

or:

```bash
rust_backtester
```

Round-specific shortcuts:

```bash
make tutorial
```

Submission and future-round shortcuts are also available once you place a `submission.log`, `submission.json`, or round data into `datasets/round1/`, `datasets/round2/`, and so on.

Useful optional variables for any `make` backtest target:

```bash
make tutorial DAY=-1
make submission ROUND=round1
make round3 TRADER=traders/latest_trader.py
make round2 PERSIST=1
```

Supported input formats:

- normalized dataset JSON files
- IMC day data as matching `prices_*.csv` and `trades_*.csv`
- portal submission logs such as `submission.log`

Day selection behavior:

- `DAY=-1` or `DAY=-2` runs only that day file inside the round
- `DAY=all` or omitting `DAY` runs the whole round bundle, including any submission file when present
- `make submission ROUND=round1` runs the submission dataset for `datasets/round1/` when a submission file is present

Explicit examples:

```bash
rust_backtester \
  --trader /path/to/trader.py \
  --dataset tutorial
```

```bash
rust_backtester \
  --trader /path/to/trader.py \
  --dataset datasets/tutorial
```

```bash
rust_backtester \
  --trader /path/to/trader.py \
  --dataset /path/to/submission.log
```

```bash
rust_backtester \
  --trader /path/to/trader.py \
  --dataset datasets/round1
```

Behavior:

- fast mode is the default
- the CLI prints one result row per day
- `--dataset` accepts either a path or a short alias
- when `--dataset` points to a directory, every supported dataset in that directory is run
- `prices_*.csv` files are paired automatically with matching `trades_*.csv` files from the same folder
- `latest` and `tutorial` run the full bundled tutorial round bundle: day `-2`, day `-1`, and the sample tutorial submission log
- use `--day <n>` to run only the matching day dataset within the round bundle; this excludes submission files
- normal fast runs also write `submission.log` under `runs/<backtest-id>/`
- use `--persist` or `PERSIST=1` to write replay artifacts under `runs/`
- persisted multi-day or multi-file runs also write one combined bundle at `runs/<backtest-id>/`, including a merged `submission.log`
- product output defaults to a compact summary so large product sets do not flood the terminal

Bundled dataset aliases:

- `latest`
- `tutorial`, `tut`, `tutorial-round`, `tut-round`
- `round1`, `r1`
- `round2`, `r2`
- `round3`, `r3`
- `round4`, `r4`
- `round5`, `r5`
- `round6`, `r6`
- `round7`, `r7`
- `round8`, `r8`
- `tutorial-1`, `tut-1`, `tut-d-1`
- `tutorial-2`, `tut-2`, `tut-d-2`

Product display modes:

- `--products summary` default: print a separate product table with the top product PnL contributors and an `OTHER(+N)` rollup when needed
- `--products full`: print a separate product table with every product
- `--products off`: show only the per-day total

Example output shape:

```text
trader: latest_trader.py [auto]
dataset: tutorial [default]
mode: fast
artifacts: log-only
SET             DAY    TICKS  OWN_TRADES    FINAL_PNL  RUN_DIR
D-2              -2    10000          39       118.10  runs/backtest-123-day-2-day-2
D-1              -1    10000          42       123.45  runs/backtest-123-day-1-day-1
SUB              -1     2000          18        51.20  runs/backtest-123-submission-day-1

PRODUCT        D-2        D-1        SUB
TOM          70.00      77.20      29.10
EMR          48.10      46.25      22.10
```

### Bundled Targets

```bash
make build
make build-release
make test
make install
make install-pip
make install-uv
make install-uv-editable
make backtest
make tutorial
```

Additional `round1` to `round8` and `round1-submission` to `round8-submission` targets are included in the `Makefile`. They become usable once those dataset folders contain JSON files.

## Isolated Verification

There is a Docker-based smoke path for isolated verification:

```bash
make docker-smoke
```

The Docker image builds the project in a clean container and runs the zero-argument backtest flow during image build.

## Repository Layout

- `src/` Rust backtester implementation
- `traders/latest_trader.py` bundled default trader
- `datasets/tutorial/` bundled raw IMC tutorial day CSVs and sample submission log
- `datasets/round1/` ... `datasets/round8/` placeholders for future round data
- `runs/` persisted outputs when `--persist` is used
- `runs/<backtest-id>/` combined bundle for persisted multi-day runs, including merged `submission.log`, merged `combined.log`, and `manifest.json`

## Licensing

Dual-licensed under:

- Apache-2.0: `LICENSE-APACHE`
- MIT: `LICENSE-MIT`
