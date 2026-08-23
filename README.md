<div align="center">

# foresight

**Your bash shell, finishing your sentences.**

It watches what you type, learns your habits, and quietly suggests the rest of the
line — like fish shell's autosuggestions, but built from scratch for bash, running
100% on your machine.

[![Version](https://img.shields.io/badge/version-0.5.0--beta-blue)](https://github.com/farrukh2002/foresight/releases)
[![Platform](https://img.shields.io/badge/platform-linux-informational)](#install)
[![License](https://img.shields.io/badge/no%20network-100%25%20local-brightgreen)](#how-it-works)

</div>

---

## What it does

- **Learns from you.** Every command you run trains a little model of *your* habits —
  no generic dataset, no cloud, no accounts.
- **Predicts the whole line**, not just the next word — including your file and folder
  paths under `$HOME`.
- **Stays out of your way.** One key press accepts a suggestion; keep typing and it's
  gone.
- **Never phones home.** No network calls for predictions. No ML libraries. Just a
  small background daemon and a fast in-memory model.

## Install

One line:

```bash
curl -fsSL https://raw.githubusercontent.com/farrukh2002/foresight/main/install.sh | bash
```

Then start a new shell — that's it, foresight is now watching.

<details>
<summary>Want the beta or dev builds instead of stable?</summary>

<br>

```bash
curl -fsSL https://raw.githubusercontent.com/farrukh2002/foresight/main/install.sh | FORESIGHT_CHANNEL=beta bash
```

</details>

## Using it

| Keys | What it does |
|---|---|
| `Ctrl-X Ctrl-P` | Accept the full suggested line |
| `Ctrl-X Ctrl-W` | Accept just the next word |
| `Ctrl-X Ctrl-F` | Pick from top suggestions with `fzf` (if installed) |

## Staying up to date

```bash
foresight update              # grab the latest version on your channel
foresight update --check      # just check, don't install
foresight update --enable-silent   # let it update itself quietly in the background
```

foresight ships on three channels — `stable`, `beta`, and `dev`. You stay on whichever
one you installed until you deliberately switch with `foresight update --channel beta`;
after that, updates (including silent ones) keep following that channel.

Silent updates, when enabled, are checked once a day, verified with a checksum before
installing, and only ever come from this repo.

## Config

foresight writes `~/.config/foresight/config.toml` the first time it runs. Open it up —
every option is commented inline, covering what folders it scans and how updates behave.

## Building from source

```bash
cargo build --release
```

<details>
<summary>Contributor tools (benchmarking)</summary>

<br>

Not part of the normal build or released binaries — just a way to compare foresight
against a zsh-autosuggestions-style baseline on your own trained data.

```bash
cargo build --release --features bench
foresight bench [N]
```

</details>

## Support

If foresight saves you a few keystrokes a day, consider chipping in:

[![GitHub Sponsors](https://img.shields.io/badge/GitHub%20Sponsors-farrukh2002-EA4AAA?logo=githubsponsors&logoColor=white)](https://github.com/sponsors/farrukh2002)
[![PayPal](https://img.shields.io/badge/PayPal-donate-00457C?logo=paypal&logoColor=white)](https://paypal.me/FarrukhSeyerHasan)

- **Binance UID:** `179696829`
- **UPI:** `farrukh@upi`
