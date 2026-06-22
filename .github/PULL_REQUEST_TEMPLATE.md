<!-- Keep PRs focused. Conventional Commits (`type(scope): description`); rebase, don't merge; no trailers, no attribution. See CONTRIBUTING.md. -->

## What this changes

<!-- One or two sentences. -->

## Checklist

- [ ] The local quality gate passes, and CI is green (build + test on all three
      OSes, fmt, clippy, typos, doc, semver, coverage, deny, vet).
- [ ] No new report strings — or, if the locked disc report changed, the change
      is intentional and the fixture test is re-pinned.
- [ ] Coverage floors still hold (100% lines / regions / functions on the
      library).
- [ ] No `unsafe`, and no new C / FFI dependency.
- [ ] Every commit is a Conventional Commit (`type(scope): description`, an allowed
      type) — `convco check origin/master..HEAD` passes.
- [ ] No attribution or LLM/AI-attribution words in any commit message or in this
      PR's title / body.
