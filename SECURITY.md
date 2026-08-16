# Security policy

bdinfo-rs parses **untrusted, attacker-controllable input**: the on-disc byte structures of
arbitrary Blu-ray rips and `.iso` images. The whole product guarantee is that doing so is safe, so
the bar here is higher than "no memory corruption":

- `unsafe` is `forbid`-den across the workspace, ruling out use-after-free, out-of-bounds writes,
  and type confusion at compile time. A green build *is* that guarantee.
- On top of that, bdinfo-rs holds a **no-panic, no-hang contract on hostile disc bytes**. A
  malformed playlist, clip, M2TS packet, codec header, or UDF descriptor must return an error —
  never crash, never loop forever, never read out of bounds.

Because of that contract, **a panic, hang, unbounded allocation, or out-of-bounds read triggered by
malformed input is an in-scope security issue**, not merely a bug. Every untrusted-input parser is
continuously fuzzed against exactly these failure modes.

## Supported versions

Security fixes land on the latest release of the current major line.

| Version | Supported |
|---|---|
| 4.0.x | ✅ |
| 3.x | ❌ |
| < 3.0 | ❌ |

## Reporting a vulnerability

**Report privately — do not open a public issue.**

1. **Preferred** — [GitHub Private Vulnerability Reporting](https://github.com/agentjp/bdinfo-rs/security/advisories/new):
   the repository's **Security** tab → **Report a vulnerability**. This opens an advisory visible
   only to you and the maintainers.
2. **Fallback** — email **agent@fastmail.jp**.

### What to include

- The version (`bdinfo-rs --version`) and your OS and architecture.
- Whether the input was a `BDMV` folder or an `.iso` image.
- The smallest input that triggers it, and the exact command you ran.
- The observed behaviour — panic message, hang, runaway memory — and what you expected.

> **Do not attach copyrighted disc content.** A crashing input is almost always a small malformed
> structure rather than real movie data. Send a minimized sample, a synthetic reproducer, or a
> description of the byte structure that triggers it. Reports reproducible only with commercial disc
> content cannot be accepted.
