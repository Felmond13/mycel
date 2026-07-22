# Contributing to Mycel

Thanks for your interest in Mycel! Issues and pull requests are welcome —
bug reports, docs fixes, and features alike.

## License status (read this first)

Mycel is **not** open source (yet): it is source-available under the
[Business Source License 1.1](LICENSE) — free for personal and internal
non-commercial use, no redistribution, and it automatically becomes
Apache-2.0 on July 22, 2030.

## Contribution terms — assignment of rights

**By submitting a contribution to this repository (for example by opening a
pull request), you agree that:**

1. You assign to the project owner, **Noureddine BOUKADOUM**, all rights,
   title, and interest in your contribution, including all copyright and
   related rights, to the maximum extent permitted by applicable law.
2. Where such assignment is not possible under applicable law, you instead
   grant the owner a perpetual, worldwide, irrevocable, royalty-free,
   exclusive license to use, reproduce, modify, distribute, sublicense, and
   **relicense** your contribution under any terms, including commercial
   ones.
3. You waive, to the extent permitted by law, any moral rights in your
   contribution, and you will not assert them against the owner.
4. Your contribution is your own original work and you have the right to
   submit it under these terms.

There is no separate CLA to sign — submitting the pull request *is* the
agreement. If you cannot accept these terms, please don't submit code
(issues and bug reports remain very welcome).

Note that forking this repository is permitted only to the extent strictly
necessary to prepare and submit a contribution here (see the `LICENSE`
Additional Use Grant).

## Workflow

- **Issues** — open one for bugs, questions, or feature ideas. A short
  reproduction (`myc` command + output) helps a lot.
- **Pull requests** — target the `main` branch. Keep them focused: one
  topic per PR.
- **CI must be green** — the GitHub Actions workflow runs `cargo fmt`,
  `clippy`, and the test suite; a PR is only merged once it passes.
- Run `scripts/dev.sh test` locally before pushing if you can.
