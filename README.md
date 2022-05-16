# cargo-loom

`cargo-loom` is a [Cargo subcommand] to automate running [Loom] testing workflows.

[![Crates.io](https://img.shields.io/crates/v/cargo-loom.svg)](https://crates.io/crates/cargo-loom)
[![Documentation](https://docs.rs/cargo-loom/badge.svg)][docs]
[![Build Status](https://github.com/hawkw/cargo-loom/actions/workflows/ci.yaml/badge.svg)](https://github.com/hawkw/cargo-loom/actions)

[docs]: https://docs.rs/cargo-loom

## What Does it Do?

[Loom] is a testing tool for concurrent Rust code. It runs a test many
times, permuting the possible concurrent executions of that test under
the [C11 memory model][spec].

Because Loom is an _exhaustive model checker_, the same test may be re-run a
very large number of times in order to explore every unique path through the
program under test allowed by the model. Although Loom uses [state reduction
techniques][cdschecker] to avoid combinatorial explosion, a non-trivial Loom
test may run for 100,000s of iterations before a permutation that results in a
failure is detected. 

Loom's deterministic execution allows the specific chain of events that caused a
test to fail to be isolated and stored in a [checkpoint file], so that the
failing execution can be re-run without having to explore all the paths under
which the model succeeds. Once a failing execution has been isolated, [logging]
and [location tracking] can be enabled to aid in debugging the failing test.
However, this test debugging workflow currently requires a number of [manual
steps][checkpoint file].

This is where `cargo-loom` comes in. This crate provides a [cargo subcommand]
that automates parts of this workflow. Invoking `cargo-loom` performs the
following actions:

1. Building the test suite with `RUSTFLAGS="--cfg loom"` enabled
2. Running the test suite (with support for [`cargo test`]-style filtering) to
   discover failing tests
3. Rerunning failing tests to generate a checkpoint file for each failure case 
4. Finally, re-running those failing tests a final time with logging and
   location tracking enabled, so that the failure can be debugged
   
Checkpoint files are stored according to the hash of the build artifact for the
test suite, so when the code under test has not changed, the checkpointed
execution may be reused in future runs to display different outputs or change
execution parameters.

## Quickstart

To install `cargo-loom`, run:

```console
cargo install cargo-loom
```

Once `cargo-loom` is installed, run

```console
cargo loom
```

in a Cargo workspace that contains Loom tests, to run those tests using
`cargo-loom`.

## Command-Line Interface

`cargo loom` supports most of the same command-line options as [`cargo test`],
including test name filtering and passing additional arguments to the test
binary. For example, to run only tests defined in `my_loom_tests.rs` with names
containing `interesting_model`, run:

```console
cargo loom --test my_loom_tests interesting_model
```

Additional arguments can be passed to the test binary using `--`, similarly to
`cargo test`. For example, to pass the `--nocapture` argument to disable libtest
output capturing, run:

```console
cargo loom -- --nocapture --test-threads 1
```

The `cargo loom` CLI can also be used to configure [Loom's execution
parameters][env]. All of the supported environment variables are passed through
to the Loom execution. Additionally, they may also be set using command-line
arguments. For example, to limit the maximum duration that a Loom model will run
for and the number of thread switches per permutation, run:

```console
cargo loom --max-duration-secs 120 --max-branches 1000
```

For a complete list of supported command-line arguments, run:

```console
cargo loom --help
```

## License

This project is licensed under the [MIT license](https://github.com/hawkw/cargo-loom/LICENSE).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in `cargo-loom` by you, shall be licensed as MIT,
without any additional terms or conditions.

[Loom]: https://crates.io/crates/loom
[spec]: https://en.cppreference.com/w/cpp/atomic/memory_order
[cdschecker]: http://plrg.eecs.uci.edu/publications/toplas16.pdf
[logging]: https://docs.rs/loom/latest/loom/model/struct.Builder.html#structfield.log
[location tracking]: https://docs.rs/loom/latest/loom/model/struct.Builder.html#structfield.location
[checkpoint file]: https://docs.rs/loom/latest/loom/#debugging-loom-failures
[cargo subcommand]: https://doc.rust-lang.org/book/ch14-05-extending-cargo.html
[`cargo test`]: https://doc.rust-lang.org/cargo/commands/cargo-test.html
[env]: https://docs.rs/loom/latest/loom/model/struct.Builder.html