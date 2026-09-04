# Use XDG paths on Linux and macOS

tmnotify uses the XDG environment variables and XDG-style fallback paths on both Linux and macOS instead of using `~/Library/Application Support` on macOS. A single path policy keeps configuration, documentation, and support behavior consistent across the two officially supported operating systems; when `XDG_RUNTIME_DIR` is unavailable, runtime IPC uses a mode-0700 per-user directory under the system temporary directory.
