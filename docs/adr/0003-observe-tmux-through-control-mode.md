# Observe tmux through control mode

Each tmnotify daemon maintains one tmux control-mode connection to observe client, session, and window changes and reconciles its view periodically at low frequency. This avoids installing global tmux hooks or polling continuously; the control-mode client is infrastructure owned by tmnotify and is excluded from the attached clients and windows that receive notification replicas.
