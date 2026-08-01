app-about = Translate games and structured text with reusable project state
cli-ui-language-help = Language for help, diagnostics, progress, results, and project logs: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko, or vi
cli-progress-help = Live progress mode: auto, plain, or off
cli-mz-about = Translate an RPG Maker MZ game
cli-mv-about = Translate an RPG Maker MV game
cli-generic-about = Translate structured JSONL text
cli-init-about = Initialize or update a named translation project
cli-extract-about = Synchronize source text from the project's current input
cli-translate-about = Translate extracted text with an explicit or saved profile
cli-write-back-about = Write current translations to the project's output
cli-project-lua-about = Run one atomic database Lua program in a project
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
cli-project-lua-script-help = Atomic database Lua program to run once
cli-project-lua-arguments-help = UTF-8 argument passed to Lua arg[1..] after --
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
cli-invalid-progress = Unsupported progress mode { $value }; use auto, plain, or off.
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
log-label-phase-check-project = checking project
log-label-phase-scan-source = scanning source
log-label-phase-prepare-candidate = preparing candidate
log-label-phase-update-database = updating database
log-label-phase-publish = publishing
log-label-phase-builtin = built-in extraction
log-label-phase-rules = rules extraction
log-label-phase-lua = Lua processing
log-label-phase-planning = planning
log-label-phase-confirmed-tasks = confirming tasks
log-label-phase-no-work = no work required
log-label-phase-read-assets = reading assets
log-label-phase-plan-rpg-maker-write-back = planning RPG Maker write-back
log-label-phase-rewrite-documents = rewriting documents
log-label-phase-validate-candidate = validating candidate
log-label-task-complete = complete
log-label-task-partial = partial
log-label-task-unavailable = unavailable
log-label-task-failed = failed
error-state-applied-finalization = The result took effect, but finalization failed. Inspect the project state before retrying.
error-no-executable-extract-owner = Clearing these owners leaves no executable Extract owner, so no plan was saved.
error-plan-save-failed-applied = The command result took effect, but the new run plan was not saved. Pass the intended options explicitly next time.
error-plan-save-outcome-unknown = The command result took effect, but the run-plan commit outcome is unknown. Pass the intended options explicitly next time.
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
result-cancelled = The command was cancelled after safe finalization.
result-plan-saved = The successful run plan was saved.
log-run-started = Command { $command } started.
log-run-succeeded = Command { $command } completed successfully.
log-run-failed = Command { $command } failed.
log-run-outcome-unknown = Command { $command } ended with an unknown final outcome; follow the recovery locations in the error.
log-run-cancelled = Command { $command } was cancelled.
log-performance-counters = Performance counters: SQLite transaction-control attempts { $sqlite_control_attempted_total }; full candidate-tree validations started { $candidate_validation_started }, completed { $candidate_validation_completed }.
log-lua-script = Lua script { $identity } (SHA-256 { $fingerprint }).
log-lua-print = Lua: { $message }
log-lua-summary = Lua activity: database calls { $database_calls }, changed rows { $changed_rows }, translation calls { $translation_calls }, printed lines { $printed_lines }.
log-plan-resolved = Command { $command } resolved its plan from { $source }.
log-phase-started = Phase started: { $phase }.
log-phase-finished = Phase finished: { $phase }.
log-retry-summary = { $count ->
    [one] 1 retry was performed.
   *[other] { $count } retries were performed.
}
log-no-work = No work was required: { $reason }.
log-no-work-translation-up-to-date = translations already match the current source and profile
log-partial-result = { $count ->
    [one] 1 partial result requires attention.
   *[other] { $count } partial results require attention.
}
log-translation-task-started = Translation task { $index }/{ $total } started.
log-translation-task-finished = Translation task { $index } finished with outcome { $outcome }.
log-translation-task-diagnostic = Translation task { $index } reported a diagnostic after { $attempts } attempts: { $diagnostic }
diagnostic-title = Error [{ $code }]
diagnostic-stage = Stage: { $stage }
diagnostic-subject = Location: { $subject }
diagnostic-subject-value = { $kind ->
    [command] command { $value }
    [field] field { $value }
    [project] project { $value }
    [profile] profile { $value }
    [component] component { $value }
   *[other] { $value }
}
diagnostic-reason = Reason: { $reason }
diagnostic-impact = Impact: { $impact }
diagnostic-action = Action: { $action }
diagnostic-recovery = Recovery: { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] component { $value }
    [transaction] transaction { $value }
   *[other] { $value }
}
diagnostic-related = Related error { $index }:
diagnostic-stage-value = { $code ->
    [process_startup] Process startup
    [process_output] Process output
    [configuration] Configuration loading
    [command_preparation] Command preparation
    [project_opening] Project opening
    [init] Initialization
    [extract] Extraction
    [translate] Translation
    [write_back] Write-back
    [lua] Project Lua execution
    [model_request] Model request
    [run_plan_finalization] Run-plan finalization
    [publication] Publication
    [shutdown] Shutdown
    [logging] Project logging
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] State was not changed
    [valid_progress_preserved] Valid progress was preserved
    [result_applied_but_run_plan_not_saved] Result was applied, but the run plan was not saved
    [state_applied_but_finalization_failed] State was applied, but finalization did not complete
    [recovery_required] Recovery is required before the state can be trusted
    [outcome_unknown] The final state is unknown
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] Correct the named configuration field and retry
    [fix_input] Correct the named input and retry
    [check_path_and_permissions] Check the path, filesystem state, and permissions
    [check_project_state] Inspect the project state, correct it, and retry
    [retry_after_resolving_contention] Wait for the competing operation to finish, then retry
    [check_model_service] Check the model service response and account limits
    [preserve_recovery_artifacts] Do not delete the listed recovery artifacts; recover the output before retrying
    [retry] Retry the operation
    [report_bug] Report this ATT defect with the error code and log path
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] A required value is missing
    [extract_plan_required] No reusable Extract plan is saved; provide --builtin or --rules
    [generic_extract_required] The JSONL input no longer matches the latest Extract; run att generic extract again
    [conflicting_values] The supplied values conflict
    [invalid_syntax] The value has invalid syntax
    [invalid_encoding] The value has invalid text encoding
    [invalid_value] The value violates the required contract
    [not_found] The required object does not exist
    [busy] The resource is held by another operation
    [state_mismatch] The stored project state does not satisfy this operation
    [requirement_failed] A required precondition is not satisfied
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
    [lua_database_open_failed] The Lua host could not open the project database session
    [lua_context_creation_failed] The Lua runtime could not create the VM context
    [lua_compilation_failed] the Lua main program could not be compiled
    [lua_execution_failed] The Lua main program failed while it was running
    [lua_host_call_failed] A Lua host capability call failed
    [lua_finalization_failed] The Lua host could not finalize all bound resources
    [rules_definition_invalid] The Rules program does not satisfy the Rules definition contract
    [rules_document_read_failed] A source document required by the Rules program could not be read
    [rules_no_non_blank_match] The Rules entry produced no non-blank semantic unit
    [rules_invalid_target] The Rules entry selected a value that cannot be used as a text target
    [rules_pattern_match_failed] The Rules PCRE2 pattern could not be evaluated
    [rules_zero_width_match] The Rules pattern produced a zero-width match
    [rules_overlapping_capture] The Rules pattern produced overlapping text captures
    [rules_missing_text_capture] The required named text capture did not participate in the match
    [rules_invalid_capture_range] The Rules match or text capture is outside valid UTF-8 character boundaries
    [rules_duplicate_target] Two Rules entries claim the same physical text target
    [rules_invalid_materialization] The Rules projection recipe cannot reconstruct the source value
    [rules_snapshot_invalid] The extracted Rules groups do not form a valid asset snapshot
    [rules_snapshot_store_failed] The validated Rules extraction snapshot could not be committed
    [write_back_extraction_out_of_date] The extracted assets no longer match the current project source
    [write_back_asset_snapshot_invalid] The stored RPG Maker assets do not form a valid write-back snapshot
    [source_document_invalid] An RPG Maker source document does not satisfy the required document format
    [generic_source_document_invalid] A Generic JSONL source document does not satisfy the required format
    [write_back_mutation_invalid] A validated translation mutation cannot be applied to its frozen source location
    [write_back_output_path_invalid] A rewritten file is outside the permitted RPG Maker output tree
    [write_back_output_path_duplicate] More than one rewritten file targets the same output path
    [write_back_candidate_project_mismatch] The prepared write-back candidate belongs to a different project
    [write_back_candidate_invalid] The write-back candidate does not satisfy the required data/js tree structure
    [write_back_not_published] The write-back candidate did not replace the current output directory
    [write_back_published_with_residuals] The output was published, but one or more recovery artifacts could not be removed
    [write_back_recovery_required] The output directory requires recovery before its contents can be trusted
    [internal_invariant] An internal invariant was violated; this is an ATT defect
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] Not found
    [permission_denied] Permission denied
    [connection_refused] Connection refused
    [connection_reset] Connection reset
    [host_unreachable] Host unreachable
    [network_unreachable] Network unreachable
    [connection_aborted] Connection aborted
    [not_connected] Not connected
    [address_in_use] Address already in use
    [address_not_available] Address unavailable
    [network_down] Network down
    [broken_pipe] Broken pipe
    [already_exists] Already exists
    [would_block] Operation would block
    [not_a_directory] Not a directory
    [is_a_directory] Is a directory
    [directory_not_empty] Directory not empty
    [read_only_filesystem] Read-only filesystem
    [stale_network_file_handle] Stale network file handle
    [invalid_input] Invalid operation input
    [invalid_data] Invalid data
    [timed_out] Operation timed out
    [write_zero] Write made no progress
    [storage_full] Storage is full
    [not_seekable] Object is not seekable
    [quota_exceeded] Storage quota exceeded
    [file_too_large] File is too large for the underlying system
    [resource_busy] Resource is busy
    [executable_file_busy] Executable file is busy
    [deadlock] Operation would deadlock
    [crosses_devices] Operation crosses filesystem devices
    [too_many_links] Too many filesystem links
    [invalid_filename] Invalid filename
    [argument_list_too_long] Operating-system argument list is too long
    [interrupted] Operation was interrupted
    [unsupported] Operation is unsupported
    [unexpected_eof] Unexpected end of file
    [out_of_memory] Operating system could not allocate memory
    [other] Other operating-system error
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] Language policy term must not be blank
    [language_policy_term_surrounding_whitespace] Language policy term must not contain surrounding whitespace
    [language_policy_term_duplicate] Language policy term must not be duplicated
    [quote_repair_candidates_empty] Quote repair candidate list must not be empty
    [quote_repair_delimiter_invalid] Quote repair delimiter must not be alphanumeric, whitespace, or control
    [quote_repair_pair_duplicate] Quote repair pair must not be duplicated
    [quote_repair_delimiter_ambiguous] Quote repair delimiter must belong to exactly one pair
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
diagnostic-io-reason = Operation { $operation }: { $kind }
diagnostic-io-reason-with-os-code = Operation { $operation }: { $kind } (OS { $os_code })
diagnostic-io-reason-with-system-message = Operation { $operation }: { $kind }: { $system_message }
diagnostic-io-reason-with-os-code-and-system-message = Operation { $operation }: { $kind } (OS { $os_code }): { $system_message }
diagnostic-failure-with-detail = { $failure }: { $detail }
diagnostic-invalid-utf8 = Invalid UTF-8 at byte { $valid_up_to }, invalid length { $error_len }
diagnostic-incomplete-utf8 = Incomplete UTF-8 sequence after byte { $valid_up_to }
diagnostic-toml-failure-value = { $code ->
    [syntax] TOML syntax is invalid
    [missing_field] A required configuration field is missing
    [unknown_field] The configuration contains an unknown field
    [duplicate_field] The configuration field is declared more than once
    [type_mismatch] Expected { $expected }
    [invalid_value] The configuration value violates the field contract
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] a string
    [integer] an integer
    [boolean] a Boolean
    [string_or_boolean] a string or Boolean
    [string_array] an array of strings
    [integer_array] an array of integers
    [string_pair_array] an array of string pairs
    [table] a table
    [table_array] an array of tables
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = Invalid TOML ({ $resource }): { $failure }
diagnostic-invalid-toml-at = Invalid TOML at line { $line }, column { $column } ({ $resource }): { $failure }
diagnostic-http-no-details = The model service request failed without any public HTTP status details
diagnostic-http-status = HTTP status { $status }
diagnostic-http-retry-after = Retry-After { $seconds } seconds
diagnostic-http-provider-code = Provider error code { $code }
diagnostic-http-provider-type = Provider error type { $kind }
diagnostic-http-provider-message = Provider error message { $message }
diagnostic-http-fact-separator = ;{ " " }
diagnostic-sqlite = SQLite primary error code { $primary_code }, extended error code { $extended_code }
diagnostic-windows-status = Windows operation { $operation } failed with NTSTATUS { $status }
diagnostic-resource = { $resource }: actual { $actual }
diagnostic-resource-with-maximum = { $resource }: actual { $actual }, maximum { $maximum }
task-record-title = Translation task { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] Complete
    [partial] Partially complete
    [unavailable] Unavailable
    [execution_failed] Execution failed
    [commit_preparation_failed] Commit preparation failed
    [commit_not_applied] Commit not applied
    [commit_outcome_unknown] Commit outcome unknown
    [not_committed_after_earlier_failure] Not committed after an earlier failure
    [invalid_result] Invalid executor result sequence
    [cancelled] Cancelled
   *[other] { $state }
}
task-record-summary-with-written = `Task { $ordinal }/{ $total }` · `{ $attempts } attempts` · `Accepted { $accepted }/{ $expected }` · `Written to { $written } locations`
task-record-summary-without-written = `Task { $ordinal }/{ $total }` · `{ $attempts } attempts` · `Accepted { $accepted }/{ $expected }`
task-record-run-id-label = Run ID:
task-record-started-at-label = Started at:
task-record-duration-label = Total duration:
task-record-endpoint-label = Endpoint:
task-record-model-label = Model:
task-record-custom-parameters-heading = Custom parameters
task-record-attempts-heading = Request attempts
task-record-final-result-heading = Final result
task-record-no-request = No model request was ready to send.
task-record-empty-assistant = The model returned an empty object.
task-record-parse-error = Parse error: { $kind ->
    [thinking_empty] the thinking content is empty at line { $line }, column { $column }
   *[json] invalid model response JSON (category `{ $category }`) at line { $line }, column { $column }
}
task-record-attempt-succeeded = Attempt { $number }: succeeded; finish reason { $finish_reason }
task-record-attempt-token-usage = ; tokens `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; duration `{ $duration }`
task-record-attempt-request-id = ; request ID { $request_id }
task-record-attempt-response-id = ; response ID { $response_id }
task-record-attempt-retryable = Attempt { $number }: retryable request failure; diagnostic `{ $code }`; duration `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; retrying after `{ $duration }`
task-record-attempt-wait-completed = ; wait of `{ $duration }` completed; the next attempt did not start
task-record-attempt-wait-cancelled = ; planned wait `{ $duration }`; cancelled while waiting
task-record-attempt-failed = Attempt { $number }: request or response processing failed; diagnostic `{ $code }`; duration `{ $duration }`
task-record-attempt-cancelled = Attempt { $number }: cancelled; duration `{ $duration }`
task-record-structured-reason = Reason: { $reason }
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
task-record-rejected-heading = Not accepted:
task-record-rejected-item = { $id }: { $reason }
task-record-protocol-diagnostic = Protocol diagnostic: { $diagnostic }
task-record-unavailable-reason = Unavailable reason: { $reason }
task-record-task-diagnostic = Task diagnostic: `{ $code }`; reason { $reason }
task-record-rejection-reason = { $code ->
    [missing] Missing model output
    [duplicate] Duplicate model output
    [invalid_shape] { $detail }
    [invalid_shape_array] The translation must be a string array
    [invalid_shape_item] Translation array item { $line } must be a string
    [line_count_mismatch] Line count mismatch (expected { $expected }, actual { $actual })
    [invalid_line_text] Line { $line } contains invalid control characters
    [blank_line_mismatch] Blank state mismatch on line { $line } (expected { $expected_blank ->
        [blank] blank
       *[other] non-blank
    })
    [blank_translation] Translation is blank
    [no_natural_language_text] Translation contains no natural-language text
    [contains_byte_order_mark] Translation contains a BOM
    [placeholder_mismatch] Placeholder mismatch: { $detail }
    [unexpected_placeholder] Unexpected placeholder: { $detail }
    [placeholder_normalization_ambiguous] Placeholder normalization is ambiguous: { $detail }
    [source_residual] Source-language residue detected: { $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason is not stop: { $detail }
    [invalid_response] { $detail }
    [invalid_id] Model item { $index } has an invalid ID
    [unknown_id] Model item { $index } returned unknown ID { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] Model response could not be parsed
    [all_outputs_rejected] All model outputs failed validation
    [recoverable_request_exhausted] Retry budget for recoverable requests was exhausted
    [retry_after_exceeds_maximum] Retry-After exceeds the configured maximum wait
   *[other] { $code }
}
task-record-duration-seconds = { $value } seconds
task-record-duration-milliseconds = { $value } ms
