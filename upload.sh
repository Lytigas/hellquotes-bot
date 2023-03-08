#!/usr/bin/env bash
set -euxo pipefail

cargo build --target x86_64-unknown-linux-musl --release
scp target/x86_64-unknown-linux-musl/release/hellquotes-bot titanic:~
ssh -t titanic 'sudo mv ~/hellquotes-bot /srv/quotesbot/'
