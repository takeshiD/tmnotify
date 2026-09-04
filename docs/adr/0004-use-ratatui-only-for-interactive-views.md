# Use Ratatui only for interactive views

tmnotify uses Ratatui with Crossterm for the interactive History viewer and Attention Gate, while Toast display remains a small purpose-built renderer. Interactive views benefit from managed terminal cleanup, responsive layout, focus handling, and testable frame buffers; keeping Toasts outside the framework reduces startup and rendering overhead for the most frequent path.
