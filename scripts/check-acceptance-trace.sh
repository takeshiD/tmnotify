#!/bin/sh
set -eu

test_list=$(cargo test --locked --lib -- --list)
missing=0

while IFS='|' read -r acceptance_id test_name; do
    case "$acceptance_id" in
        '' | '#'* ) continue ;;
    esac
    if ! printf '%s\n' "$test_list" | grep -Fqx "$test_name: test"; then
        echo "$acceptance_id: traced test is missing: $test_name" >&2
        missing=1
    fi
done <<'TRACE'
TOAST-DELIVERY|daemon::reconcile::tests::deduplicates_shared_windows_and_excludes_control_clients
TOAST-FOLLOW|daemon::reconcile::tests::attach_detach_and_move_recreate_without_replaying_enter
TOAST-LAYOUT|toast::tests::all_six_placements_anchor_and_grow_inward
TOAST-STACKING|toast::tests::order_capacity_and_waiting_are_per_window
TOAST-NARROW|toast::tests::narrow_breakpoints_are_truthful
TOAST-ANIMATION|toast::tests::animation_batches_windows_and_drops_quantized_duplicates
TOAST-UPDATE|daemon::tests::keyed_upsert_reuses_id_does_not_restart_duplicate_and_restarts_change
TOAST-TIMEOUT|daemon::tests::no_display_and_attention_pause_toast_timeout
TOAST-JUMP|daemon::tests::direct_jump_closes_only_after_successful_commit
ATTENTION-SOURCE|notification::tests::attention_requires_a_valid_source_pane
ATTENTION-QUEUE|daemon::tests::attention_is_global_and_queued_by_priority_fifo
ATTENTION-GLOBAL|daemon::reconcile::tests::deduplicates_shared_windows_and_excludes_control_clients
ATTENTION-KEYS|ui::attention::tests::keys_are_global_and_other_input_is_ignored
ATTENTION-LOSS|daemon::tests::direct_jump_rejects_missing_source_and_stale_commit
HISTORY-RESTORE|ui::tests::terminal_modes_are_restored_during_unwind
HISTORY-OUTPUT|history::tests::plain_is_cell_bounded_and_ndjson_is_complete_one_object_per_line
HISTORY-SCOPE|history::tests::default_scope_and_sort_are_current_server_updated_descending
HISTORY-HIDE|history::tests::hide_is_reversible_and_history_jump_preserves_lifecycle_ordering
HISTORY-CLEAR|history::tests::clear_is_scoped_and_each_selector_removes_only_matching_rows
HISTORY-DISABLED|history::tests::disabled_history_does_not_touch_the_database
HISTORY-JUMP|ui::history::tests::jump_requires_source_and_confirms_other_server
HOOK-INSTALL|hooks::tests::claude_install_is_idempotent_and_preserves_unrelated_configuration
HOOK-SYNC|hooks::tests::sync_updates_relocated_handlers_without_creating_other_scopes
HOOK-REMOVE|hooks::tests::remove_deletes_only_owned_handlers
HOOK-MIXED|hooks::tests::codex_inline_hooks_require_explicit_mixed_permission
HOOK-STATUS|doctor::tests::reports_private_paths_reachable_daemon_and_unknown_trust
HOOK-AUXILIARY|hooks::tests::receiver_swallows_unknown_malformed_oversized_and_submission_failures
RELIABILITY-RACE|protocol::tests::cache_replays_only_an_identical_payload
RELIABILITY-STALE|platform::tests::runtime_socket_rejects_non_socket_and_symlink_entries
RELIABILITY-INPUT|protocol::tests::decoder_rejects_a_partial_frame_as_soon_as_it_is_too_large
RELIABILITY-CONTENT|notification::tests::removes_ansi_and_replaces_controls
RELIABILITY-ARGV|render::tests::token_is_cryptographic_length_and_debug_is_redacted
RELIABILITY-RENDER|daemon::reconcile::tests::partial_success_retries_only_after_backoff_and_all_exhaustion_closes
RELIABILITY-SQLITE|history::tests::busy_retries_are_bounded
RELIABILITY-DAEMON|daemon::tests::render_and_daemon_boundaries_record_explicit_close_reasons
RELIABILITY-BACKPRESSURE|daemon::runtime::tests::per_connection_limit_backpressures_without_aborting_accepted_requests
RELIABILITY-FORCED-SHUTDOWN|daemon::runtime::tests::renderer_aware_server_honors_forced_shutdown_during_cleanup
RELIABILITY-DOCTOR|doctor::tests::system_probe_is_read_only_and_runs_without_tmux_or_daemon
TRACE

[ "$missing" -eq 0 ] || exit 1
echo 'section 24 automated acceptance trace is complete'
