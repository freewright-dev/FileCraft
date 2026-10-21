# Agent instructions

Guidance for AI coding agents (Claude Code and others) working in this repository.

## Reading and editing files

- Always use the agent's built-in file tools to read, search and change files (for Claude Code:
  Read, Grep, Glob, Edit and Write).
- Never read files with shell commands (`cat`, `head`, `tail`, `sed -n`, `type`, `Get-Content`).
- Never edit or create files with shell commands (`sed -i`, `awk`, `echo >`, heredocs, `cp` over
  sources, inline Python or PowerShell scripts).
- Use the shell only for things the file tools cannot do: `cargo`, `git` and `gh`.

## One commit per fix

- Each fix gets its own commit (and its own branch and pull request). Don't bundle unrelated
  fixes, and don't spread one fix across several commits.
- Use Conventional Commit messages (`fix:`, `feat:`, `docs:`, `chore:` …). Explain the cause and
  the fix in the body, and close the issue with `Fixes #N`.
- The commit includes everything for that fix: code, tests and documentation.
- `main` only takes reviewed pull requests. Push a branch and open a PR; never push to `main`.

## Update the docs with every change

Before committing, update whatever the change affects:

- `CHANGELOG.md`: an entry under `[Unreleased]` in the right
  [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) section (`Added`, `Changed`, `Fixed`,
  `Security` …). Include the audit ID when there is one (for example `FC-03`).
- `docs/user-guide.md`: when behavior, settings or shortcuts change.
- `docs/architecture.md`: when modules or their responsibilities change.
- `README.md`: when features or requirements change.

## Before committing

- `cargo test` and `cargo clippy --all-targets` must pass.
- A bug fix comes with a regression test. Check that the test fails without the fix.
