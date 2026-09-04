app-about = Translate games and structured text with reusable project state
cli-test-about = Check the distribution configuration and every LLM Client
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
cli-ownership-export-about = Export text ownership for every extracted RPG Maker unit
cli-translation-export-about = Export source text, current translations, and state for every extracted unit
cli-manual-check-about = Check a manual translation TOML file without changing the project
cli-manual-apply-about = Apply filled and valid manual translations
cli-project-lua-about = Run a Lua script against the project database
cli-project-name-help = Stable project name
cli-init-path-help = Input root directory; an existing project can reuse its last successful path
cli-source-language-help = Source language ID
cli-target-language-help = Target language ID
cli-builtin-help = Use ATT's built-in RPG Maker text locations
cli-rules-help = Replace the RPG Maker extraction rules with this TOML definition; an empty rule list disables them
cli-dialogue-rules-help = Replace the MV dialogue-name projection used with Builtin
cli-profile-help = Translation profile ID; omit it to reuse the last successful profile
cli-terms-help = Replace the project's terminology resource
cli-placeholders-help = Replace the project's placeholder resource
cli-project-lua-script-help = Lua script to run against the project database
cli-project-lua-arguments-help = UTF-8 argument passed to Lua arg[1..] after --
cli-manual-file-help = Manual translation TOML file
cli-jsonl-file-help = JSONL export file
cli-retry-rejected-help = Retry saved Rejected candidates
cli-manual-selection-help = Export selection: pending (default), rejected, or all
cli-manual-ids-help = Export entries matching the natural IDs in this JSONL file
cli-layout-rules-help = Load and save WriteBack layout rules from a TOML file
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
plan-source-explicit = explicit input
plan-source-project-state = project state
plan-source-product-default = product behavior
notice-init-reuse-path = No source path was provided; reusing the last successful path: { $path }.
notice-extract-reuse-owners = No extraction scope was provided; reusing the last successful plan: { $owners }.
notice-translate-reuse-profile = No profile was provided; reusing the last successful profile: { $profile }.
notice-no-model-request = All translation units are current; no model request was needed in this run.
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
progress-no-work = No work is needed
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
result-translate-completed = Translation run finished: { $project } (Profile: { $profile })
result-translate-status = Status: { $status }
result-translate-status-value = { $status ->
    [no_work] no work
    [complete] complete
    [incomplete] incomplete
   *[other] __ATT_FALLBACK__
}
result-translate-summary = Translation: { $total } planned tasks, { $started } started, { $not_started } not started; { $complete } complete, { $partial } partial, { $unavailable } unavailable, { $failed } failed, { $cancelled } cancelled; wrote { $written } locations, { $remaining } remaining, including { $rejected } rejected
result-translate-convergence = State convergence: { $retained } retained, { $invalidated } invalidated, { $not_applicable } not applicable, { $reused } reused
result-write-back-completed = Write-back complete: { $project }
result-project-lua-completed = Project Lua execution complete: { $project }
result-output-directory = Output directory: { $path }
result-write-back-summary = Write-back: { $translated } translated units, { $original } source units
result-generic-extract-unchanged = Generic input unchanged: { $files } files, { $groups } groups, { $units } units
result-generic-extract-updated = Generic input updated: { $files } files, { $groups } groups, { $units } units; preserved { $preserved } translations and cleared { $cleared }
result-generic-translate-summary = Generic translation: { $total } planned tasks, { $started } started, { $not_started } not started; { $complete } complete, { $partial } partial, { $unavailable } unavailable, { $failed } failed, { $cancelled } cancelled; { $planned_units } planned units, { $remaining_units } remaining units, including { $rejected_units } rejected, cleared { $cleared }, reused { $reused }, accepted { $accepted }, wrote { $written }, conflicts { $conflicted }, response problems { $problems }
result-generic-write-back-summary = Generic write-back: { $translated } translated units, { $original } source units retained
result-run-log = Run log: { $path }
result-test-configuration = Configuration: { $status ->
    [passed] passed
   *[failed] failed
}
result-test-client = LLM { $client }: { $status ->
    [passed] passed
   *[failed] failed
} ({ $protocol }, { $stream ->
    [streaming] streaming
   *[non_streaming] non-streaming
})
result-test-summary = Summary: { $passed }/{ $total } passed, { $failed } failed, { $skipped } not run
translate-incomplete-object = Translate run for project { $project }
translate-incomplete-rpg-maker-reason = { $partial } partial tasks, { $unavailable } unavailable tasks, { $not_started } not started, { $protocol } protocol problems, and { $exhausted } exhausted requests; request admission {
    $admission ->
        [stopped] stopped
       *[open] remained open
    }; { $remaining_decisions } decisions and { $remaining_locations } locations remain, including { $rejected_locations } rejected
translate-incomplete-generic-reason = { $partial } partial tasks, { $unavailable } unavailable tasks, { $not_started } not started, { $exhausted } exhausted requests; request admission {
    $admission ->
        [stopped] stopped
       *[open] remained open
    }; { $remaining_units } remaining units, including { $rejected_units } rejected, { $conflicted } write conflicts, and { $problems } response problems
translate-incomplete-help = Read the task diagnostics in this run log, fix repeatable problems, and run Translate again; use Manual for a small remainder
translate-incomplete-rejected-help = Read the task diagnostics in this run log; retry rejected content with --retry-rejected, or export it with manual export --selection rejected and handle it through Manual
result-cancelled = The command was cancelled after safe finalization.
result-plan-saved = The successful run plan was saved.
log-run-started = Command { $command } started.
log-run-succeeded = Command { $command } completed successfully.
log-run-failed = Command { $command } failed.
log-run-outcome-unknown = Command { $command } ended with an unknown final outcome; follow the diagnostic before retrying.
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
log-translation-task-finished = Translation task { $index } finished with outcome { $outcome }. { $provider_status ->
    [present] Upstream provider: { $provider }.
   *[missing] Upstream provider: not provided.
}
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
log-phase-name = { $phase ->
    [check_project] Project check
    [scan_source] Source scan
    [prepare_candidate] Candidate preparation
    [update_database] Database update
    [publish] Publication
    [builtin] Builtin extraction
    [builtin_documents] Builtin document scan
    [builtin_work_units] Builtin text unit extraction
    [builtin_commit] Builtin commit
    [rules] Rules extraction
    [rules_documents] Rules document scan
    [rules_matches] Rules matching
    [rules_commit] Rules commit
    [lua] Lua execution
    [planning] Translation task planning
    [confirmed_tasks] Translation task confirmation
    [read_assets] Project content reading
    [plan_rpg_maker_write_back] WriteBack planning
    [rewrite_documents] Document rewriting
    [validate_candidate] Candidate validation
   *[other] __ATT_FALLBACK__
}
log-task-outcome-value = { $outcome ->
    [complete] completed
    [partial] partially completed
    [unavailable] unavailable
    [failed] failed
    [not_committed_after_earlier_failure] not committed after an earlier failure
    [cancelled] cancelled
   *[other] ended without a recognized result
}
diagnostic-object = Object: { $subject }
diagnostic-error-heading = Error:
diagnostic-warning-heading = Warning:
diagnostic-explanation = Reason: { $reason }
diagnostic-impact = Impact: { $impact }
diagnostic-resolution = Action: { $action }
diagnostic-related = { $relation ->
    [cleanup] Cleanup also failed:
    [rollback] Rollback also failed:
    [discard] Discarding the candidate also failed:
    [finalization] Finalization also failed:
    [shutdown] Shutdown also failed:
    [observability] Result presentation or recording also failed:
   *[other] A related operation also failed:
}
diagnostic-impact-value = { $effect ->
    [unchanged] Business state was not changed
    [progress_preserved] Previously confirmed progress was preserved; the indicated content was not completed
    [applied] The related business result has taken effect
    [applied_run_plan_not_saved] The business result has taken effect, but this run plan was not saved
    [applied_finalization_failed] The business result has taken effect, but required finalization did not complete
    [recovery_required] The result is known, but the indicated recovery site must be handled first
    [outcome_unknown] Whether this operation took effect cannot be confirmed; do not retry or remove recovery artifacts before following the action
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] Correct the named configuration field and retry
    [fix_input] Correct the named input and retry
    [fix_placeholder_rules] Correct the indicated Placeholder rule and retry
    [review_translation] Review the highlighted translation; use Manual to revise it if needed
    [review_disabled_rules] If this is expected, no action is needed; otherwise add valid rules to the indicated file and run Extract again
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
    [empty_text_capture] The text capture is empty
    [rules_owner_disabled] The selected Rules file uses rule = []; Rules was disabled and its extracted assets were removed
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
    [stdout_write_failed] Standard output could not be written
    [stderr_write_failed] Standard error could not be written
    [stdout_flush_failed] Standard output could not be flushed
    [stderr_flush_failed] Standard error could not be flushed
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
    [not_regular_file] The existing target is not a regular file
    [wrong_publisher_instance] The publication token belongs to a different publisher instance
    [journal_corrupt] The publication recovery journal is invalid or incomplete
    [unexpected_artifact] An unexpected filesystem artifact blocks the operation
    [interactive_session_already_open] Another interactive SQLite session is already active
    [backup_incomplete] The SQLite backup did not reach a completed state
    [request_serialization_failed] The model request could not be serialized
    [http_client_build_failed] The model-service HTTP client could not be created
    [dns_resolution_failed] DNS resolution failed
    [tcp_connection_failed] The TCP connection failed
    [request_send_failed] The HTTP request could not be sent
    [response_read_failed] The HTTP response could not be read
    [tls_handshake_failed] The TLS handshake failed
    [connect_timed_out] The TCP connection timed out
    [read_timed_out] Reading the HTTP response timed out
    [request_timed_out] The HTTP request exceeded its total timeout
    [response_decode_failed] The HTTP response could not be decoded
    [redirect_rejected] The HTTP redirect was rejected
    [response_parsing_failed] The model response is not valid JSON
    [model_stream_invalid_json] A model stream event is not valid JSON
    [model_stream_invalid_utf8] The model stream contains invalid UTF-8
    [model_stream_error_event] The model stream returned a service error event
    [model_stream_unclosed_event] An SSE event was not closed by a blank line
    [model_stream_missing_finish] The Chat model stream is missing finish_reason
    [model_stream_missing_responses_terminal] The Responses model stream is missing its terminal event
    [model_stream_event_type_mismatch] The SSE event name and JSON type do not match
    [model_stream_duplicate_choice] The model stream repeated the same choice
    [model_stream_choice_after_finish] The Chat stream sent response-changing fields after finish
    [model_stream_unexpected_done] The Responses model stream returned an unexpected [DONE]
    [response_json_invalid] The assistant response is not valid JSON
    [response_shape_invalid] The assistant JSON has an invalid root or response shape
    [response_id_invalid] A response item has an invalid output ID
    [response_id_unexpected] The response contains an output ID that was not requested
    [response_id_duplicate] The response contains the same output ID more than once
    [response_id_missing] The response is missing a requested output ID
    [response_translation_not_array] The translation value must be an array of strings
    [response_translation_item_not_string] A translation array item is not a string
    [response_echo_shape_invalid] The echoed source object does not match the requested source/translation shape
    [response_echo_source_item_not_string] An echoed source array item is not a string
    [response_translation_blank] The returned translation is blank
    [response_translation_text_invalid] The returned translation contains a disallowed line break, NUL, or byte-order mark
    [response_placeholder_snapshot_invalid] The Placeholder snapshot used to validate this response is invalid
    [response_placeholder_identity_or_count_mismatch] The translation changed the required Placeholder identities or counts
    [response_placeholder_missing] The translation is missing a required control token
    [response_placeholder_unexpected] The translation contains an unexpected control token
    [response_placeholder_order_mismatch] The translation changed the required control-token order
    [response_placeholder_binding_mismatch] The translation changed how required Placeholders bind to the text
    [response_placeholder_boundary_mismatch] The translation added or removed a required Placeholder boundary
    [response_placeholder_reserved_token] The translation contains a reserved Placeholder token
    [response_placeholder_ambiguous] A returned Placeholder cannot be matched to one required token unambiguously
    [response_control_token_invalid] The returned control-token structure is invalid
    [response_text_segment_count_mismatch] The response changed the required number of text segments
    [response_text_segment_shape_mismatch] The response changed the required text-segment structure
    [response_line_count_mismatch] The translation array has the wrong number of items
    [response_line_text_invalid] A translation array item contains text that cannot be accepted
    [response_blank_line_mismatch] The translation did not preserve the required blank and non-blank array slots
    [response_source_residual] The accepted translation still contains source-language text and needs review
    [response_finish_requires_review] The model stopped for a non-final reason; the returned result needs review
    [response_thinking_empty] The required think field is empty or contains only whitespace
    [response_no_usable_output] The assistant response contains no usable output
    [response_all_outputs_rejected] Every output in the assistant response was rejected
    [invalid_response_contract] The model response does not satisfy the required response contract
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
diagnostic-http-route-direct = Direct connection (no proxy)
diagnostic-http-route-proxy = Via explicit proxy { $proxy }
diagnostic-retry-after = Retry-After: { $seconds } seconds
diagnostic-provider-code = Provider code: { $code }
diagnostic-provider-type = Provider type: { $kind }
diagnostic-provider-message = Provider message: { $message }
diagnostic-json-position = line { $line }, column { $column }
diagnostic-input-field = Field: { $field }
diagnostic-input-failure = { $code ->
    [syntax] Invalid TOML syntax
    [missing_field] A required field is missing
    [unknown_field] This field is not allowed by the current format
    [duplicate_field] The field is duplicated
    [type_mismatch] The field has the wrong type
    [invalid_value] The field value is invalid
   *[other] __ATT_FALLBACK__
}
diagnostic-expected-type = Expected type: { $expected ->
    [string] string
    [integer] integer
    [boolean] boolean
    [string_or_boolean] string or boolean
    [string_array] array of strings
    [integer_array] array of integers
    [table] table
    [table_array] array of tables
    [array] array
    [object] object
   *[other] __ATT_FALLBACK__
}
diagnostic-response-item = response item { $item }
diagnostic-array-item = array item { $item }
diagnostic-token-position = control-token position { $position }
diagnostic-text-segment = text segment { $segment }
diagnostic-post-finish-fields = fields after finish: { $fields }
diagnostic-expected-actual = expected { $expected }, received { $actual }
diagnostic-placeholder-rule-file = Placeholder rule { $number } in { $path }
diagnostic-placeholder-rule-project = Placeholder rule { $number } in the current project
manual-exported = Exported { $entries } entries to { $path }
manual-checked = Valid { $valid }, unfilled { $unfilled }, errors { $errors }
manual-applied = Applied { $applied }, unfilled { $unfilled }, errors { $errors }
manual-value = { $code ->
    [translation_byte_order_mark] translation item { $line } contains a BOM (U+FEFF)
    [remove_byte_order_mark] Remove the BOM (U+FEFF) character from the translation
    [keep_placeholders] Restore the source Placeholders in the translation, preserving their counts, required order, and text slots
    [invalid_source_line] source item { $line } contains a line break or NUL
    [invalid_translation_line] translation item { $line } contains a line break or NUL
    [fixed_length] fixed translation requires { $expected } items; found { $actual }
    [fixed_blank_slot] fixed translation item { $line } must remain blank
    [rerun_export] Rerun manual export
    [rerun_export_without_controls] Rerun manual export and do not put line breaks or NUL in array items
    [rerun_export_then_fill] Rerun manual export, then fill in the translation
    [resolve_temporary_then_rerun_export] Resolve the displayed fixed temporary path, remove any leftover object there, then rerun manual export
    [resolve_published_backup_cleanup] Both exports have been applied; verify them, then remove the displayed fixed backup file
    [keep_exported_type] Keep the type written by manual export
   *[other] __ATT_FALLBACK__
}
task-record-title = Translation task
task-record-final-result-heading = Final result
task-record-final-status = Status: { $state ->
    [complete] complete and commit confirmed
    [partial] partially complete and commit confirmed
    [unavailable_rejected_committed] Unavailable; rejected candidates saved
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
task-record-provider = Upstream provider: { $provider }
task-record-provider-unavailable = Upstream provider: not provided
task-record-requested = Requested translations: { $requested }
task-record-accepted-written = Accepted items: { $accepted } (IDs: { $ids }); locations written: { $written }
task-record-accepted-outcome-unknown = Validated items: { $accepted } (IDs: { $ids }); database commit outcome cannot be confirmed
task-record-unaccepted = Items not accepted: { $unaccepted } (IDs: { $ids })
task-record-task-diagnostic = Task diagnostic
