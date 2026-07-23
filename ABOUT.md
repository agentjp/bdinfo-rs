# About this project

## Why BDInfo

Blu-ray is a finished format and the spec is very unlikely to change
again. That made it a near-perfect target: a real, non-trivial, widely used tool with a frozen
specification and a fixed, verifiable output — you can tell exactly when a port is correct.

## The experiment

bdinfo-rs is a port of BDInfo from .NET to Rust, written and maintained through LLMs. The chatter
about porting Bun from Zig to Rust sparked the curiosity; the question was how far
partially-supervised, closed-loop LLM-driven development could be pushed — and, more interestingly,
whether it could **maintain** a project rather than just generate one.

That question is why the engineering bar here looks the way it does. Nothing in the gate is
decoration: 100% coverage, zero surviving mutants, fuzzing on every untrusted-input parser, and a
locked byte-exact output contract exist because they are the mechanism that lets an autonomous loop
change code without silently breaking it. The process is the product; the code is what falls out of
the other end.

## What that means for you

Blu-ray is a sprawling format with a long tail of mastering quirks, and the only real proof is
discs in the wild. bdinfo-rs aims to be the reference implementation of the BDInfo report, and
divergences from the original are deliberate, documented, and verified against the codec
specifications — see [DIFFERENCES.md](DIFFERENCES.md).

If it gets something wrong on one of your discs, that is the single most useful thing you can send:
open an [issue](https://github.com/agentjp/bdinfo-rs/issues/new/choose) with the details and it goes
straight back into the loop. Issues get handled.

## The pieces

| Crate | What it is |
|---|---|
| [`bdinfo-rs-core`](https://crates.io/crates/bdinfo-rs-core) | The analyzer: discovery, MPLS/CLPI/index, M2TS demux, 13 codec scanners, UDF 2.50, report renderer |
| [`bdinfo-rs`](https://crates.io/crates/bdinfo-rs) | The command-line front-end |
| [`bdinfo-rs-gui`](https://crates.io/crates/bdinfo-rs-gui) | The native desktop app (iced) |
| [`@bdinfo-rs/wasm`](https://www.npmjs.com/package/@bdinfo-rs/wasm) | The same analyzer compiled to WebAssembly |

The three front-ends are thin shells. All of the parsing, and the report itself, lives in the core
crate — which is why the output is byte-identical across all of them.
