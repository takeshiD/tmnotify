# Repository instructions

This file is the source of truth for repository-wide instructions followed by coding agents.

## Agent configuration

- Keep repository-owned rules under `.agents/rules/` and project-specific skills under `.agents/skills/`.
- Keep `.claude/rules` and `.claude/skills` as symbolic links to those canonical directories.
- Keep the root `CLAUDE.md` as a symbolic link to this file so Codex and Claude Code receive the same repository instructions.
- Resolve every repository-owned symbolic link within the repository and verify that it remains valid from a fresh checkout.

## Required project context

Before planning, implementing, or reviewing product behavior, read:

1. [`CONTEXT.md`](CONTEXT.md) for the domain language;
2. [`tmnotify-design.md`](tmnotify-design.md) for the accepted implementation design; and
3. [`.agents/rules/architecture.md`](.agents/rules/architecture.md) for ADR-derived guardrails.

Use the terms defined in `CONTEXT.md` consistently. The design document defines the current product contract, while accepted ADRs are authoritative for decisions intended to survive implementation rewrites. If a requested change conflicts with an accepted ADR, stop implementation and propose a superseding ADR first.

## Worktree-only change workflow

Keep the primary checkout on `main` for synchronization and inspection. Make every tracked-file change in a dedicated worktree under `.worktrees/<branch-name>`; do not implement directly in the primary checkout.

Use a short lowercase kebab-case branch name with a change category such as `add-`, `fix-`, `docs-`, `cicd-`, or `chore-`. The worktree directory name must exactly match the branch name.

Create a worktree from the latest remote `main` when network access is available:

```bash
git fetch origin main
git worktree add .worktrees/<branch-name> \
  --no-track -b <branch-name> origin/main
```

When working from the local `main` intentionally, replace `origin/main` with `main`. Before editing, verify both invariants:

```bash
test "$(git branch --show-current)" = "$(basename "$PWD")"
git rev-parse --show-toplevel
```

The repository root reported by the second command must be inside `.worktrees/`. Keep build artifacts and task-specific temporary files in the same worktree. Use one branch and one worktree per independent change.

After a change is merged, update the primary checkout, remove the completed worktree with `git worktree remove`, and delete the merged local branch.

## Engineering rules

- Preserve the one-binary product boundary unless an accepted ADR supersedes it.
- Prefer a small set of deep, responsibility-oriented modules. Do not expose internal scheduler, lifecycle, policy, tmux protocol, or storage mechanics as shallow cross-module APIs.
- Keep provider-specific schemas at ingress adapters; normalize them before they reach scheduling and persistence.
- Treat notification text and provider input as untrusted. Never interpolate them into shell commands or emit them as terminal control sequences.
- Bound queues, buffers, retries, request sizes, shutdown waits, and caches explicitly.
- Keep network access, telemetry, automatic update checks, and hidden provider trust manipulation out of the product.
- Preserve stdout for command results and machine-readable output; write diagnostics to stderr. Interactive terminal modes must restore terminal state on every recoverable exit path.

## Verification

Run checks proportionate to the change from inside its worktree. For Rust changes, the normal baseline is:

```bash
cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Add focused tests for affected behavior. For interactive terminal work, cover state transitions and rendered output at constrained terminal sizes. For tmux integration, prefer fake adapters and deterministic protocol fixtures; reserve real-tmux tests for lifecycle and integration boundaries.

Before completing a change, review the diff against every applicable section of `.agents/rules/architecture.md` and its linked ADRs.
