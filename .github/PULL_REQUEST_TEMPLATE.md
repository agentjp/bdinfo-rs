<!-- Keep PRs focused. One imperative single-sentence subject per commit; no body, no trailers, no attribution. -->

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
- [ ] Commit messages follow the one-sentence rule (imperative subject, no body,
      no trailers, no attribution).
