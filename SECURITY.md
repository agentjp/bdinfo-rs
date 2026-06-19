# Security Policy

bdinfo-rs parses **untrusted, attacker-controllable input**: the on-disc byte
structures of arbitrary Blu-ray rips and `.iso` images. The entire product
guarantee is that doing so is safe, so the security bar here is higher than "no
memory corruption":

- `unsafe` is `forbid`-den across the whole workspace, so classic memory-safety
  bugs (use-after-free, out-of-bounds writes, type confusion) are ruled out at
  compile time — a green build *is* that guarantee.
- On top of that, bdinfo-rs promises a **no-panic / no-hang contract on hostile
  disc bytes**. A malformed playlist, clip, M2TS packet, codec header, or UDF
  descriptor must return an error — never crash, never loop forever, never read
  out of bounds.

Because of that contract, a **panic, hang, unbounded allocation, or
out-of-bounds read triggered by malformed input is an in-scope security issue**
for this project, not merely a bug. The parser is continuously fuzzed against
exactly these failure modes.

## Supported Versions

bdinfo-rs follows SemVer; security fixes land on the latest `1.0.x` release.

| Version | Supported          |
| ------- | ------------------ |
| 1.0.x   | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a Vulnerability

**Please report security issues privately — do not open a public issue.**

1. **Preferred — GitHub Private Vulnerability Reporting.** On the repository, go
   to the **Security** tab → **Report a vulnerability**. This opens a private
   advisory visible only to you and the maintainers.
2. **Fallback — email** **agent@fastmail.jp**.

### What to include

- The bdinfo-rs version (`bdinfo-rs --version`) and your OS / architecture.
- Whether the input was a `BDMV` folder or an `.iso` image.
- The smallest input that triggers the issue, and the exact command you ran.
- The observed behavior (panic message, hang, runaway memory, …) and what you
  expected instead.

> **Do not attach copyrighted disc content.** A crashing input is almost always
> a small, malformed structure rather than real movie data — send a minimized
> sample, a synthetic reproducer, or a description of the byte structure that
> triggers it. Reports that can only be reproduced with commercial disc content
> cannot be accepted.
