# Snap Coin Miner
## Installation
To install Snap Coin Node, run:
```bash
cargo install snap-coin-miner
```
Make sure you have cargo, and rust installed.

## General Information
By default the miner pulls the `toml` config from the running directory as `miner.toml`

## Usage
```bash
snap-coin-node <args>
```
Available arguments:

1. `--config <path>`
Specifies path to a `toml` miner config.

## Configuration
The miner configuration is stored in a toml file that is structured like this:
```toml
[node]
address = "<your Snap Coin API node address and port (eg. 127.0.0.1:3003)>"

[miner]
public = "<your public wallet address>"

[threads]
count = <amount of threads to run on, -1 for max>
```