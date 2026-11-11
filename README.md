# nargo for nemi

Prebuilt `nargo` of [`nemi/parallel-contract-functions@ddef36c`](https://github.com/nemi-fi/noir/tree/ddef36c070348333a6f63a6aca0b745c295fd321): the Noir commit of aztec 6.0.0's nargo (`5a7ee9bf`) plus the parallel contract compile and per-function cache. Built with `cargo build --release -p nargo_cli` and `GIT_COMMIT=5a7ee9bf5ed8973076df7bb0d2b723024db09ae7 GIT_DIRTY=false`, so its artifacts are byte-for-byte those of aztec's nargo.

- `nargo-x86_64-unknown-linux-gnu.tar.gz`: Linux x86_64, glibc (Ubuntu 24.04)
- `SHA256SUMS`

Used by nemi's `packages/contracts/scripts/compile-fast.js`.
