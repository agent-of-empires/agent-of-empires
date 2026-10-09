#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_TEMP:?hosted runner temp directory is required}"
: "${RUNNER_OS:?hosted runner OS is required}"
: "${GITHUB_STEP_SUMMARY:?GitHub step summary is required}"
[[ "${GITHUB_ACTIONS:-}" == true && "${RUNNER_ENVIRONMENT:-}" == github-hosted ]] || {
  echo "native Creating proofs require a disposable GitHub-hosted runner" >&2
  exit 1
}
case "$RUNNER_OS" in
  Linux) [[ $# -eq 0 ]] ;;
  macOS) [[ $# -eq 2 && "$1" == --features && "$2" == e2e-tests ]] ;;
  *) echo "unsupported hosted native proof OS: $RUNNER_OS" >&2; exit 1 ;;
esac

export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export AOE_CREATE_SYSCALL_DIAGNOSTICS=1
export HOME="$RUNNER_TEMP/aoe-hosted-creating-home"
export XDG_CONFIG_HOME="$HOME/.config"
mkdir -p "$XDG_CONFIG_HOME"
log="$RUNNER_TEMP/hosted-creating-$RUNNER_OS.log"
exec > >(tee "$log") 2>&1
build_artifacts="$(cargo build "$@" --bin aoe --message-format=json)"
aoe_binary="$(printf '%s\n' "$build_artifacts" | jq -r '
  select(.reason == "compiler-artifact" and .target.name == "aoe"
    and .profile.test == false and .executable != null) | .executable')"
[[ -n "$aoe_binary" && "$aoe_binary" != *$'\n'* ]]
test -x "$aoe_binary"
export AOE_NATIVE_CREATE_EXECUTABLE="$(cd "$(dirname "$aoe_binary")" && pwd -P)/$(basename "$aoe_binary")"
test -x "$AOE_NATIVE_CREATE_EXECUTABLE"
echo "actual bootstrap: $AOE_NATIVE_CREATE_EXECUTABLE"
artifacts="$(cargo test "$@" --no-run --lib --message-format=json)"
test_binary="$(printf '%s\n' "$artifacts" | jq -r '
  select(.reason == "compiler-artifact" and .profile.test == true
    and .target.name == "agent_of_empires" and .executable != null) | .executable')"
[[ -n "$test_binary" && "$test_binary" != *$'\n'* ]]
test -x "$test_binary"
cases=(
  session::runner_journal::bootstrap::tests::hosted_managed_launch_parent_loss_before_ack_releases_only_after_original_death
  tui::home::pollers::hosted_tests::hosted_force_refusal_dialog_keeps_original_purge_pending
  session::runner_journal::native_create::tests::hosted_create_original_native_same_g_and_changed_profile_refusal
  session::runner_journal::native_create::tests::hosted_create_pre_target_cancel_original_retirement_and_same_g_undo
  session::runner_journal::native_create::tests::hosted_create_changed_goal_refuses_target_before_effect
  session::runner_journal::native_create::tests::hosted_create_complete_commitment_and_private_secrets
  session::builder::tests::hosted_creating_checkout_publication_and_managed_launch_preserve_receipts
  session::builder::tests::hosted_creating_remote_branch_tracking_and_original_withdrawal
  session::builder::tests::hosted_creating_hook_exit23_retains_error_and_original_withdrawal_outcome
  session::builder::tests::hosted_creating_changed_resources_refuse_deletion_and_keep_claims
  session::builder::tests::hosted_creating_preexisting_checkout_is_never_undone
  tui::creation_poller::hosted_tests::hosted_creating_lost_receiver_and_late_cancel_retry_without_effects
)
if [[ "$RUNNER_OS" == Linux ]]; then
  cases+=(
    session::runner_journal::native_create::tests::hosted_create_cancel_retires_actual_original_descendants
    session::builder::tests::hosted_creating_branch_unlink_ack_survives_late_retirement_failure
    session::runner_journal::native_create::external_domain_test::hosted_create_actual_external_ipc_stays_protected_after_cli_exit
    session::builder::tests::hosted_creating_normal_git_is_quiescent_before_unstarted_attach
  )
fi
failed=0
for proof in "${cases[@]}"; do
  found=0
  while IFS= read -r line; do
    if [[ "$line" == "$proof: test" ]]; then found=$((found + 1)); fi
  done < <("$test_binary" "$proof" --exact --ignored --list)
  if [[ "$found" -ne 1 ]]; then
    echo "expected exactly one ignored native proof: $proof (found $found)" >&2
    exit 1
  fi
  echo "::group::Hosted Creating proof: $proof"
  if "$test_binary" "$proof" --exact --ignored --nocapture --test-threads=1; then
    printf -- '- PASS `%s` (%s)\n' "$proof" "$RUNNER_OS" >> "$GITHUB_STEP_SUMMARY"
  else
    failed=$((failed + 1))
    printf -- '- FAIL `%s` (%s)\n' "$proof" "$RUNNER_OS" >> "$GITHUB_STEP_SUMMARY"
  fi
  echo "::endgroup::"
done
[[ "$failed" -eq 0 ]]
