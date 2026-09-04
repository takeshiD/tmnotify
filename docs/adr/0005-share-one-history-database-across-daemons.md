# Share one History database across per-server daemons

tmnotify runs one live-state daemon per tmux server while all of a user's daemons write to one SQLite History database in WAL mode. This preserves server isolation for queues and display replicas while allowing a unified durable history; writes and migrations use short transactions, bounded busy retries, and never block the scheduler or renderer event loops.
