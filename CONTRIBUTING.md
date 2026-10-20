# Contributing to spk

## Developer Certificate of Origin

Every commit must carry a `Signed-off-by:` trailer certifying the
contribution under the [DCO](DCO.md), the same mechanism used by the
Linux kernel, Docker, and most CNCF projects. This is checked
automatically on every pull request (`.github/workflows/dco.yml`) and
PRs with unsigned commits will fail CI.

Sign off by committing with `-s`:

```
git commit -s -m "..."
```

This appends a trailer matching your commit author identity:

```
Signed-off-by: Your Name <your@email>
```

To fix commits that are missing it:

```
git commit --amend -s          # the most recent commit
git rebase --signoff <base>     # a whole range of commits
```

## AI-assisted contributions

Parts of this codebase were written with AI coding assistants
(disclosed via `Co-Authored-By:` trailers in the relevant commits,
same convention used across every Smech Labs repo). This is not
hidden and contributions written with AI assistance are welcome under
the same terms as any other contribution — but you are certifying the
DCO either way, which means you're taking the same responsibility for
the code's origin and correctness regardless of how the first draft
was produced. Understand what you're submitting before signing off on
it; don't sign off on code you haven't read.

## License

spk is MIT-licensed (see [LICENSE](LICENSE)). By contributing, you
agree your contribution is provided under the same license.
