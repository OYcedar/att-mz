app-about = Translate games and structured text with reusable project state
cli-ui-language-help = Language for help, diagnostics, progress, results, and project logs: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko, or vi
cli-mz-about = Translate an RPG Maker MZ game
cli-mv-about = Translate an RPG Maker MV game
cli-generic-about = Translate structured JSONL text
cli-init-about = Initialize or update a named translation project
cli-extract-about = Synchronize source text from the project's current input
cli-translate-about = Translate extracted text with an explicit or saved profile
cli-write-back-about = Write current translations to the project's output
cli-manual-about = Manage manual translations with an editable TOML file
cli-manual-export-about = Export entries that currently need manual translation
cli-manual-check-about = Check a manual translation TOML file without changing the project
cli-manual-apply-about = Apply filled and valid manual translations
cli-project-lua-about = Run a Lua script against the project database
cli-project-name-help = Stable project name
cli-init-path-help = Input root directory; an existing project can reuse its last successful path
cli-source-language-help = Source language ID
cli-target-language-help = Target language ID
cli-dialogue-width-help = Maximum full-width characters per dialogue line
cli-scrolling-width-help = Maximum full-width characters per scrolling-text line
cli-help-width-help = Maximum full-width characters per help or description line
cli-builtin-help = Use ATT's built-in RPG Maker text locations
cli-rules-help = Replace the RPG Maker extraction rules with this TOML definition; an empty rule list disables them
cli-dialogue-rules-help = Replace the MV dialogue-name projection used with Builtin
cli-profile-help = Translation profile ID; omit it to reuse the last successful profile
cli-terms-help = Replace the project's terminology resource
cli-placeholders-help = Replace the project's placeholder resource
cli-project-lua-script-help = Lua script to run against the project database
cli-project-lua-arguments-help = UTF-8 argument passed to Lua arg[1..] after --
cli-manual-file-help = Manual translation TOML file
cli-usage-heading = Usage:
cli-commands-heading = Commands:
cli-options-heading = Options:
cli-arguments-heading = Arguments:
cli-options-metavar = OPTIONS
cli-command-metavar = COMMAND
cli-print-help = Print help
cli-print-version = Print version
cli-blank-value = The value must not be blank.
cli-invalid-positive-integer = The value must be a positive integer.
cli-invalid-ui-language-argument = --ui-language contains an invalid language tag: { $value }.
cli-unsupported-ui-language-argument = --ui-language requests an unsupported language: { $value }.
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE contains an invalid language tag: { $value }.
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE requests an unsupported language: { $value }.
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE is not valid Unicode.
cli-unexpected-argument = Unexpected argument: { $value }.
cli-missing-required-argument = Missing required argument: { $value }.
cli-invalid-value = Invalid value { $value } for { $argument }.
cli-error-heading = Error:
cli-try-help = For more information, use --help.
cli-missing-value = A value is required for { $argument }.
cli-missing-subcommand = A command is required.
cli-argument-conflict = { $argument } cannot be used with the other provided arguments.
cli-wrong-number-of-values = The wrong number of values was provided for { $argument }.
cli-invalid-utf8 = A command-line argument is not valid Unicode.
cli-parse-failure = The command line could not be parsed.
error-no-executable-extract-owner = Clearing these owners leaves no executable Extract owner, so no plan was saved.
plan-source-explicit = explicit input
plan-source-project-state = project state
plan-source-product-default = product behavior
notice-init-reuse-path = No source path was provided; reusing the last successful path: { $path }.
notice-extract-reuse-owners = No extraction scope was provided; reusing the last successful plan: { $owners }.
notice-translate-reuse-profile = No profile was provided; reusing the last successful profile: { $profile }.
notice-owner-disabled = Owner { $owner } was disabled and removed from future automatic plans.
warning-rules-command-non-string-skipped = Warning: Rules rule { $rule_number } skipped { $skipped_count } non-string command parameters (source { $source_file }, code={ $command_code }, parameter={ $parameter }, type={ $actual_type }).
warning-manual-layout-required = Warning: manual line-break review is required at { $locations } (region={ $region }, max_fullwidth_chars={ $max_fullwidth_chars }).
notice-no-model-request = All translation units are current; no model request was needed in this run.
notice-manual-layout = { $count ->
    [one] 1 unit needs a manual line-break review.
   *[other] { $count } units need a manual line-break review.
}
notice-log-degraded = Project logging is unavailable or degraded; the command will continue and its exit status is unchanged.
notice-task-records-degraded = Translation task records are unavailable or degraded; the command will continue and its exit status is unchanged.
progress-init-check-project = Checking project state
progress-init-scan-source = Scanning the game source
progress-init-build-candidate = Building the project candidate
progress-init-converge-database = Converging the project database
progress-init-publish = Publishing the initialized project
progress-save-run-plan = Saving the successful run plan
progress-extract-owner = Extract owner: { $owner }
progress-extract-documents = Scanning documents
progress-extract-builtin = Builtin work units
progress-extract-rules = Rules definitions
progress-extract-commit = Committing extracted assets
progress-generic-init = Initializing the Generic project
progress-generic-extract = Scanning Generic JSONL input
progress-translate-planning = Planning translation tasks
progress-translate-confirmed = Confirmed translation tasks
progress-translate-no-work = No model request is needed
progress-project-lua = Running the project Lua program
progress-write-back-read-assets = Reading accepted assets
progress-write-back-planning = Planning document rewrites
progress-write-back-documents = Rewritten documents
progress-write-back-validate-candidate = Validating the output candidate
progress-write-back-publish = Publishing output; interruption will wait for a confirmed outcome
progress-finalizing = Finalizing required resources
progress-safe-stopping = Stopping safely; preserving the last confirmed progress
result-init-completed = Initialization complete: { $project }
result-init-created = Project state: created
result-init-unchanged = Project state: unchanged
result-init-updated = Project state: updated
result-init-stale-owners = Re-extraction required: { $owners }
result-extract-completed = Extraction complete: { $project }
result-translate-completed = Translation complete: { $project } (Profile: { $profile })
result-translate-summary = Translation: { $total } tasks; { $complete } complete, { $partial } partial, { $unavailable } unavailable; wrote { $written } locations, { $remaining } remaining
result-translate-convergence = State convergence: { $retained } retained, { $invalidated } invalidated, { $not_applicable } not applicable, { $reused } reused
result-write-back-completed = Write-back complete: { $project }
result-project-lua-completed = Project Lua execution complete: { $project }
result-output-directory = Output directory: { $path }
result-write-back-summary = Write-back: { $translated } translated units, { $original } source units; auto-wrapped { $auto_wrapped }, inserted { $breaks } line breaks and { $indents } full-width indents; { $manual } need manual layout
result-generic-extract-unchanged = Generic input unchanged: { $files } files, { $groups } groups, { $units } units
result-generic-extract-updated = Generic input updated: { $files } files, { $groups } groups, { $units } units; preserved { $preserved } translations and cleared { $cleared }
result-generic-translate-summary = Generic translation: { $total } tasks; { $complete } complete, { $partial } partial, { $unavailable } unavailable; cleared { $cleared }, reused { $reused }, accepted { $accepted }, wrote { $written }, conflicts { $conflicted }, response problems { $problems }
result-generic-write-back-summary = Generic write-back: { $translated } translated units, { $original } source units retained
result-symbol-repair-summary = Symbol repair: attempted { $attempted } units, repaired { $repaired }, skipped internally { $skipped }, replaced { $replacements } symbols
result-cancelled = The command was cancelled after safe finalization.
result-plan-saved = The successful run plan was saved.
log-run-started = Command { $command } started.
log-run-succeeded = Command { $command } completed successfully.
log-run-failed = Command { $command } failed.
log-run-outcome-unknown = Command { $command } ended with an unknown final outcome; follow the recovery locations in the error.
log-run-cancelled = Command { $command } was cancelled.
log-performance-counters = Performance counters: SQLite transaction-control attempts { $sqlite_control_attempted_total }; full candidate-tree validations started { $candidate_validation_started }, completed { $candidate_validation_completed }.
log-lua-print = Lua: { $message }
log-plan-resolved = Command { $command } resolved its plan from { $source }.
log-phase-started = Phase started: { $phase }.
log-retry-summary = { $count ->
    [one] 1 retry was performed.
   *[other] { $count } retries were performed.
}
log-translation-task-started = Translation task { $index }/{ $total } started.
log-translation-task-finished = Translation task { $index } finished with outcome { $outcome }.
log-run-recovery-required = Command { $command } ended in a state that requires recovery; follow the recovery locations in the diagnostic.
log-phase-completed = Phase completed: { $phase }.
log-phase-stopped = { $outcome ->
    [failed] Phase failed: { $phase }.
    [cancelled] Phase cancelled: { $phase }.
   *[other] Phase stopped: { $phase }.
}
log-cancellation-requested = Cancellation requested after { $confirmed } of { $total } items were confirmed.
log-cancellation-requested-indeterminate = Cancellation requested after { $confirmed } items were confirmed; the total is not known.
log-run-plan-finalized = { $result ->
    [saved] The run plan was saved.
    [not_saved] The run plan was not saved.
    [saved_finalization_failed] The run plan was saved, but finalization failed.
    [outcome_unknown] The final state of the run plan is unknown.
   *[other] Run-plan finalization stopped without a recognized result.
}
log-translation-finished = { $result ->
    [not_started] Translation did not start.
    [no_work] Translation finished with no work required.
    [complete] Translation completed.
    [incomplete] Translation finished with incomplete work.
    [failed] Translation failed.
    [cancelled] Translation was cancelled.
   *[other] Translation stopped without a recognized result.
}
log-publication-started = Publication started for output root { $path }.
log-publication-finished = { $result ->
    [published] Publication completed.
    [not_published] Publication did not modify the output.
    [recovery_required] Publication stopped and requires recovery.
    [outcome_unknown] The final publication state is unknown.
   *[other] Publication stopped without a recognized result.
}
log-project-log-degraded = The project log degraded; { $failure_kinds } failure categories were recorded.
log-task-outcome-value = { $outcome ->
    [complete] completed
    [partial] partially completed
    [unavailable] unavailable
    [failed] failed
    [not_committed_after_earlier_failure] not committed after an earlier failure
    [cancelled] cancelled
   *[other] ended without a recognized result
}
diagnostic-location = Location: { $subject }
diagnostic-explanation = Reason: { $reason }
diagnostic-resolution = Action: { $action }
diagnostic-related = Related error { $index }:
diagnostic-resolution-value = { $code ->
    [fix_configuration] Correct the named configuration field and retry
    [fix_input] Correct the named input and retry
    [fix_placeholder_rules] Correct the indicated Placeholder rule and retry
    [adjust_manual_layout] Manually adjust line breaks and layout at the indicated locations for the stated display width
    [check_path_and_permissions] Check the path, filesystem state, and permissions
    [check_project_state] Inspect the project state, correct it, and retry
    [resolve_contention] Wait for the competing operation to finish, then retry
    [check_model_service] Check the model service response and account limits
    [preserve_recovery_artifacts] Do not delete the listed recovery artifacts; recover the output before retrying
    [retry] Retry the operation
    [report_bug] Report this ATT defect and describe what you were doing
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] A required value is missing
    [generic_extract_required] The JSONL input no longer matches the latest Extract; run att generic extract again
    [conflicting_values] The supplied values conflict
    [invalid_syntax] The value has invalid syntax
    [invalid_encoding] The value has invalid text encoding
    [invalid_value] The value violates the required contract
    [not_found] The required object does not exist
    [state_mismatch] The stored project state does not satisfy this operation
    [unsupported_windows_code_page] The Windows code page is not UTF-8
    [transaction_rolled_back] The transaction failed and its changes were rolled back
    [transaction_outcome_unknown] The transaction ended without a confirmed commit or rollback result
    [finalization_failed] The operation result exists but finalization failed
    [rollback_failed] The primary operation failed and rollback also failed
    [external_service_rejected] The external service rejected the request
    [external_service_unavailable] The external service is unavailable
    [executor_closed] The execution service is shutting down or already closed
    [concurrent_shutdown] Another caller is already shutting down the executor
    [executor_state_poisoned] The executor lifecycle state is poisoned
    [worker_spawn_failed] The operating system could not create the worker thread
    [worker_channel_closed] The worker command channel closed before finalization completed
    [worker_panicked] A worker terminated unexpectedly
    [reparse_point_forbidden] The path contains a reparse point that cannot be trusted
    [non_local_volume] The path is not on a local fixed volume
    [non_ntfs_volume] The path is not on an NTFS volume
    [case_sensitive_directory] The directory has case-sensitive name semantics
    [lock_cancelled] Waiting for the required lock was cancelled
    [target_already_exists] The destination already exists
    [file_identity_changed] The file identity changed during the operation
    [invalid_path] The path is not a valid target for this operation
    [wrong_publisher_instance] The publication token belongs to a different publisher instance
    [journal_corrupt] The publication recovery journal is invalid or incomplete
    [unexpected_artifact] An unexpected filesystem artifact blocks the operation
    [interactive_session_already_open] Another interactive SQLite session is already active
    [backup_incomplete] The SQLite backup did not reach a completed state
    [request_serialization_failed] The model request could not be serialized
    [response_parsing_failed] The model response is not valid JSON
    [invalid_response_contract] The model response does not satisfy the required response contract
    [transport_failed] The HTTP transport failed before a valid response arrived
    [lua_compilation_failed] the Lua main program could not be compiled
    [lua_execution_failed] The Lua main program failed while it was running
    [rules_pattern_match_failed] The Rules PCRE2 pattern could not be evaluated
    [rules_zero_width_match] The Rules pattern produced a zero-width match
    [rules_overlapping_capture] The Rules pattern produced overlapping text captures
    [rules_missing_text_capture] The required named text capture did not participate in the match
    [rules_invalid_capture_range] The Rules match or text capture is outside valid UTF-8 character boundaries
    [write_back_candidate_invalid] The write-back candidate does not satisfy the required data/js tree structure
    [write_back_recovery_required] The output directory requires recovery before its contents can be trusted
    [already_exists] The target object already exists
    [cancelled] The operation was cancelled
    [concurrent_modification] The project state changed concurrently
    [duplicate_identifier] An identifier is duplicated
    [extraction_out_of_date] The stored extraction no longer matches the current source
    [invalid_content] The content violates the required contract
    [manual_layout_required] Manual line-break or layout adjustment is required
    [operation_failed] The operation failed
    [placeholder_projection_failed] Placeholder projection did not preserve the required structure
    [profile_not_found] The selected translation Profile does not exist
    [recovery_required] Recovery is required before the result can be trusted
    [resource_limit] A required resource limit was reached
    [resource_limit_exceeded] The operation exceeded a backend resource limit
    [source_snapshot_mismatch] The source no longer matches the stored snapshot
    [unavailable] The requested work is temporarily unavailable
    [internal_invariant] An internal invariant was violated; this is an ATT defect
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] Language policy term must not be blank
    [language_policy_term_surrounding_whitespace] Language policy term must not contain surrounding whitespace
    [language_policy_term_duplicate] Language policy term must not be duplicated
    [language_id_blank] Language ID must not be blank
    [language_id_surrounding_whitespace] Language ID must not contain surrounding whitespace
    [language_id_uses_underscore] Language ID must use hyphens between subtags
    [language_id_invalid_syntax] Language ID must satisfy RFC 5646 syntax
    [language_id_invalid_registry_tag] Language ID contains an invalid registry subtag
    [language_id_canonicalization_failed] Language ID cannot be canonicalized
    [language_id_undefined_primary_language] Language ID must define a primary language
    [language_id_duplicate] Language ID must be unique
    [language_catalog_empty] At least one source language module is required
    [url_invalid] Value must be a valid URL
    [url_credentials_forbidden] URL must not contain credentials
    [url_fragment_forbidden] URL must not contain a fragment
    [url_scheme_unsupported] URL scheme must be http or https
    [api_key_blank] API key must not be blank
    [api_key_surrounding_whitespace] API key must not contain surrounding whitespace
    [api_key_invalid_header] API key cannot be represented as an HTTP header value
    [strict_json_invalid] Value must be strict JSON (line={ $line }, column={ $column })
    [json_object_required] Value must be a JSON object
    [reserved_request_field] Field is owned by the request protocol and cannot be overridden
    [proxy_must_be_false_or_url] Proxy must be false or a complete http/https URL
    [pem_path_duplicate] PEM path must be unique
    [runtime_maximum_exceeded] Value exceeds runtime maximum (actual={ $actual }, maximum={ $maximum })
    [value_surrounding_whitespace] Value must not contain surrounding whitespace
    [value_blank] Value must not be blank
    [path_blank] Path must not be empty
    [positive_required] Value must be greater than zero (actual={ $actual })
    [usize_range_exceeded] Value exceeds this platform's usize range (actual={ $actual })
    [u32_range_exceeded] Value exceeds u32 range (actual={ $actual })
    [duplicate_profile_id] Translation profile ID must be unique
    [selected_profile_invalid] Selected translation profile has invalid structure or field types
    [referenced_client_not_found] Referenced LLM client does not exist
   *[other] __ATT_FALLBACK__
}
diagnostic-http-status = HTTP status { $status }
diagnostic-retry-after = Retry-After: { $seconds } seconds
diagnostic-provider-code = Provider code: { $code }
diagnostic-provider-type = Provider type: { $kind }
diagnostic-provider-message = Provider message: { $message }
diagnostic-json-position = line { $line }, column { $column }
diagnostic-placeholder-rule-file = Placeholder rule { $number } in { $path }
diagnostic-placeholder-rule-project = Placeholder rule { $number } in the current project
manual-exported = Exported { $entries } entries to { $path }
manual-checked = Valid { $valid }, unfilled { $unfilled }, errors { $errors }
manual-applied = Applied { $applied }, unfilled { $unfilled }, errors { $errors }
manual-issue = { $object }: { $reason }; { $help }.
manual-value = { $code ->
    [invalid_source_line] source item { $line } contains a line break or NUL
    [invalid_translation_line] translation item { $line } contains a line break or NUL
    [fixed_length] fixed translation requires { $expected } items; found { $actual }
    [fixed_blank_slot] fixed translation item { $line } must remain blank
    [rerun_export] Rerun manual export
    [rerun_export_without_controls] Rerun manual export and do not put line breaks or NUL in array items
    [rerun_export_then_fill] Rerun manual export, then fill in the translation
    [keep_exported_type] Keep the type written by manual export
   *[other] __ATT_FALLBACK__
}
task-record-title = Translation task
task-record-final-result-heading = Final result
task-record-final-status = Status: { $state ->
    [complete] complete and commit confirmed
    [partial] partially complete and commit confirmed
    [unavailable] unavailable; project unchanged
    [execution_failed] execution failed; not committed
    [commit_preparation_failed] commit preparation failed; definitely not applied
    [commit_not_applied] transaction definitely not applied
    [commit_outcome_unknown] commit outcome unknown
    [not_committed_after_earlier_failure] not committed because an earlier task failed
    [invalid_result] invalid executor result sequence; not committed
    [cancelled] cancelled; not committed
   *[other] { $state }
}
task-record-accepted-written = Accepted: { $accepted } items; written to { $written } actual locations
task-record-accepted-outcome-unknown = Validated: { $accepted } items; database commit outcome cannot be confirmed
task-record-task-diagnostic = Task diagnostic
