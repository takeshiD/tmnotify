# Organize the implementation around deep modules

tmnotify uses a small set of top-level modules aligned with product responsibilities instead of exposing separate state, scheduler, lifecycle, and policy layers. The tmux seam accepts desired display plans and hides control-mode events, pane identities, command sequencing, and reconciliation behind production and fake adapters; History remains a concrete SQLite module tested with temporary databases until a second storage implementation creates a real seam.
