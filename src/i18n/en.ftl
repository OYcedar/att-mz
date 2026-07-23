app-about = Translate RPG Maker games with reusable project state
cli-config-help = Strict TOML configuration file for this run
cli-ui-language-help = Language for help, diagnostics, progress, results, and project logs: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko, or vi
cli-progress-help = Live progress mode: auto, plain, or off
cli-mz-about = Translate an RPG Maker MZ game
cli-mv-about = Translate an RPG Maker MV game
cli-init-about = Initialize or update a named game project
cli-extract-about = Extract source text using an explicit or saved owner plan
cli-translate-about = Translate extracted text with an explicit or saved profile
cli-write-back-about = Write accepted translations back to the game
cli-project-name-help = Stable project name
cli-init-path-help = RPG Maker game root; an existing project can reuse its last successful path
cli-source-language-help = Source language ID
cli-target-language-help = Target language ID
cli-dialogue-width-help = Maximum full-width characters per dialogue line
cli-scrolling-width-help = Maximum full-width characters per scrolling-text line
cli-help-width-help = Maximum full-width characters per help or description line
cli-builtin-help = Use ATT's built-in RPG Maker text locations
cli-rules-help = Replace the Rules owner with this TOML definition; an empty rule list disables it
cli-dialogue-rules-help = Replace the MV dialogue-name projection used with Builtin
cli-lua-help = Replace the phase Lua program; a zero-byte file clears it
cli-profile-help = Translation profile ID; omit it to reuse the last successful profile
cli-terms-help = Replace the project's terminology resource
cli-placeholders-help = Replace the project's placeholder resource
cli-usage-heading = Usage:
cli-commands-heading = Commands:
cli-options-heading = Options:
cli-arguments-heading = Arguments:
cli-options-metavar = OPTIONS
cli-command-metavar = COMMAND
cli-print-help = Print help
cli-print-version = Print version
cli-missing-config = Missing required configuration path --config <FILE>.
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
log-label-phase-plan-standard = planning standard write-back
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
notice-translate-reuse-lua = No Lua option was provided; reusing the last successful Translate Lua selection.
notice-write-back-reuse-lua = No Lua option was provided; reusing the last successful WriteBack Lua program.
notice-write-back-standard-only = No WriteBack Lua program is configured; running Standard only.
notice-owner-disabled = Owner { $owner } was disabled and removed from future automatic plans.
notice-lua-cleared = The { $phase } Lua program was cleared; it will not run this time.
notice-no-model-request = All standard translation units are current; Standard made no model request this run.
notice-manual-layout = { $count ->
    [one] 1 unit needs a manual line-break review.
   *[other] { $count } units need a manual line-break review.
}
notice-log-degraded = Project logging is unavailable or degraded; the command will continue and its exit status is unchanged.
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
progress-extract-lua = Running the Extract Lua program
progress-extract-commit = Committing extracted assets
progress-translate-planning = Planning translation tasks
progress-translate-confirmed = Confirmed translation tasks
progress-translate-no-work = No model request is needed
progress-write-back-read-assets = Reading accepted assets
progress-write-back-planning = Planning document rewrites
progress-write-back-documents = Rewritten documents
progress-write-back-lua = Running the WriteBack Lua program
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
result-translate-standard = Standard translation: { $total } tasks; { $complete } complete, { $partial } partial, { $unavailable } unavailable; wrote { $written } locations, { $remaining } remaining
result-translate-convergence = State convergence: { $retained } retained, { $invalidated } invalidated, { $not_applicable } not applicable, { $reused } reused
result-write-back-completed = Write-back complete: { $project }
result-output-directory = Output directory: { $path }
result-write-back-standard = Standard write-back: { $translated } translated units, { $original } source units; auto-wrapped { $auto_wrapped }, inserted { $breaks } line breaks and { $indents } full-width indents; { $manual } need manual layout
result-lua-executed = Lua: executed
result-lua-not-executed = Lua: not executed
result-cancelled = The command was cancelled after safe finalization.
result-plan-saved = The successful run plan was saved.
result-translate-plan-sources = This successful run plan was saved. Profile source: { $profile_source }; Lua source: { $lua_source }.
log-run-started = Command { $command } started.
log-run-succeeded = Command { $command } completed successfully.
log-run-failed = Command { $command } failed.
log-run-outcome-unknown = Command { $command } ended with an unknown final outcome; follow the recovery locations in the error.
log-run-cancelled = Command { $command } was cancelled.
log-performance-counters = Performance counters: SQLite transaction-control attempts { $sqlite_control_attempted_total }; full candidate-tree validations started { $candidate_validation_started }, completed { $candidate_validation_completed }.
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
    [process_output] Process output
   *[other] { $fallback }
}
diagnostic-impact-value = { $code ->
   *[other] { $fallback }
}
diagnostic-action-value = { $code ->
   *[other] { $fallback }
}
diagnostic-failure-value = { $code ->
   *[other] { $fallback }
}
diagnostic-io-kind-value = { $code ->
   *[other] { $fallback }
}
diagnostic-configuration-rule-value = { $code ->
   *[other] { $fallback }{ $facts }
}
