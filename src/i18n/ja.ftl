app-about = 再利用可能なプロジェクト状態でゲームと構造化テキストを翻訳します
cli-ui-language-help = ヘルプ、診断、進捗、結果、プロジェクトログの言語: ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko、vi
cli-mz-about = RPG Maker MZ ゲームを翻訳します
cli-mv-about = RPG Maker MV ゲームを翻訳します
cli-generic-about = 規定の JSONL テキストを翻訳します
cli-init-about = 名前付き翻訳プロジェクトを初期化または更新します
cli-extract-about = プロジェクトの現在の入力から原文を同期します
cli-translate-about = 明示または保存済み Profile で抽出済み原文を翻訳します
cli-write-back-about = 現在の訳文をプロジェクトの出力へ書き込みます
cli-manual-about = 編集可能な TOML で手動翻訳を管理します
cli-manual-export-about = 現在手動翻訳が必要な項目を出力します
cli-manual-check-about = プロジェクトを変更せず手動翻訳 TOML を検査します
cli-manual-apply-about = 入力済みで有効な手動翻訳を適用します
cli-project-lua-about = プロジェクトデータベースに対して Lua スクリプトを実行します
cli-project-name-help = 安定したプロジェクト名
cli-init-path-help = 入力ルートディレクトリ。既存プロジェクトでは前回成功時のパスを再利用できます
cli-source-language-help = 原文の言語 ID
cli-target-language-help = 翻訳先の言語 ID
cli-dialogue-width-help = 会話行あたりの最大全角文字数
cli-scrolling-width-help = スクロールテキスト行あたりの最大全角文字数
cli-help-width-help = ヘルプまたは説明行あたりの最大全角文字数
cli-builtin-help = ATT 内蔵の RPG Maker テキスト位置を使用します
cli-rules-help = RPG Maker 抽出ルールをこの TOML 定義で置換します。空のルール一覧で無効になります
cli-dialogue-rules-help = Builtin と併用する MV 会話名投影を置換します
cli-profile-help = 翻訳 Profile ID。省略すると前回成功した Profile を再利用します
cli-terms-help = プロジェクトの用語リソースを置換します
cli-placeholders-help = プロジェクトの Placeholder リソースを置換します
cli-project-lua-script-help = プロジェクトデータベースに対して実行する Lua スクリプト
cli-project-lua-arguments-help = -- の後で Lua arg[1..] に渡す UTF-8 引数
cli-manual-file-help = 手動翻訳 TOML ファイル
cli-usage-heading = 使用法:
cli-commands-heading = コマンド:
cli-options-heading = オプション:
cli-arguments-heading = 引数:
cli-options-metavar = オプション
cli-command-metavar = コマンド
cli-print-help = ヘルプを表示します
cli-print-version = バージョンを表示します
cli-blank-value = 値を空にすることはできません。
cli-invalid-positive-integer = 値は正の整数でなければなりません。
cli-invalid-ui-language-argument = --ui-language の言語タグが無効です: { $value }。
cli-unsupported-ui-language-argument = --ui-language で未対応の言語が指定されました: { $value }。
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE の言語タグが無効です: { $value }。
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE で未対応の言語が指定されました: { $value }。
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE は有効な Unicode ではありません。
cli-unexpected-argument = 予期しない引数です: { $value }。
cli-missing-required-argument = 必須引数がありません: { $value }。
cli-invalid-value = { $argument } の値 { $value } は無効です。
cli-error-heading = エラー:
cli-try-help = 詳細については --help を使用してください。
cli-missing-value = { $argument } には値が必要です。
cli-missing-subcommand = コマンドを指定してください。
cli-argument-conflict = { $argument } は指定済みのほかの引数と同時に使用できません。
cli-wrong-number-of-values = { $argument } に指定された値の数が正しくありません。
cli-invalid-utf8 = コマンドライン引数が有効な Unicode ではありません。
cli-parse-failure = コマンドラインを解析できませんでした。
error-no-executable-extract-owner = 消去後に実行可能な Extract owner がないため、プランは保存されませんでした。
plan-source-explicit = 明示入力
plan-source-project-state = プロジェクト状態
plan-source-product-default = 製品動作
notice-init-reuse-path = 元パスが指定されなかったため、前回成功したパスを再利用します: { $path }。
notice-extract-reuse-owners = 抽出範囲が指定されなかったため、前回成功したプランを再利用します: { $owners }。
notice-translate-reuse-profile = Profile が指定されなかったため、前回成功した Profile を再利用します: { $profile }。
notice-owner-disabled = owner { $owner } を無効にし、今後の自動プランから削除しました。
warning-rules-command-non-string-skipped = 警告：Rules ルール { $rule_number } は文字列ではない command パラメーターを { $skipped_count } 件スキップしました（ソース { $source_file }、code={ $command_code }、parameter={ $parameter }、型 { $actual_type }）。
warning-manual-layout-required = 警告：{ $locations } の改行を手動で確認してください（region={ $region }、max_fullwidth_chars={ $max_fullwidth_chars }）。
notice-no-model-request = すべての翻訳単位が最新のため、今回はモデルへのリクエストを行いませんでした。
notice-manual-layout = { $count } 単位で改行の手動確認が必要です。
notice-log-degraded = プロジェクトログを利用できないか劣化しています。コマンドは継続し、終了状態には影響しません。
notice-task-records-degraded = 翻訳タスク記録を利用できないか劣化しています。コマンドは継続し、終了状態には影響しません。
progress-init-check-project = プロジェクト状態を確認しています
progress-init-scan-source = ゲームソースを走査しています
progress-init-build-candidate = プロジェクト候補を構築しています
progress-init-converge-database = プロジェクトデータベースを収束しています
progress-init-publish = 初期化済みプロジェクトを公開しています
progress-save-run-plan = 成功した実行プランを保存しています
progress-extract-owner = 抽出 owner: { $owner }
progress-extract-documents = 文書を走査しています
progress-extract-builtin = Builtin 作業単位
progress-extract-rules = Rules 定義
progress-extract-commit = 抽出資産をコミットしています
progress-generic-init = Generic プロジェクトを初期化しています
progress-generic-extract = Generic JSONL 入力を走査しています
progress-translate-planning = 翻訳タスクを計画しています
progress-translate-confirmed = 確認済みの翻訳タスク
progress-translate-no-work = モデル呼び出しは不要です
progress-project-lua = プロジェクト Lua プログラムを実行しています
progress-write-back-read-assets = 承認済み資産を読み込んでいます
progress-write-back-planning = 文書書き換えを計画しています
progress-write-back-documents = 文書を書き換えました
progress-write-back-validate-candidate = 出力候補を検証しています
progress-write-back-publish = 出力を公開しています。中断後も確定結果を待ちます
progress-finalizing = 必須の終了処理を実行しています
progress-safe-stopping = 安全に停止しています。最後に確認した進捗を保持します
result-init-completed = 初期化完了: { $project }
result-init-created = プロジェクト状態: 作成済み
result-init-unchanged = プロジェクト状態: 変更なし
result-init-updated = プロジェクト状態: 更新済み
result-init-stale-owners = 再抽出が必要です: { $owners }
result-extract-completed = 抽出完了: { $project }
result-translate-completed = 翻訳完了: { $project }（Profile: { $profile }）
result-translate-summary = 翻訳: タスク { $total }、完全 { $complete }、部分 { $partial }、利用不可 { $unavailable }。{ $written } 箇所を書き込み、残り { $remaining } 箇所
result-translate-convergence = 状態収束: 保持 { $retained }、無効化 { $invalidated }、非該当 { $not_applicable }、再利用 { $reused }
result-write-back-completed = 書き戻し完了: { $project }
result-project-lua-completed = プロジェクト Lua 実行完了: { $project }
result-output-directory = 出力ディレクトリ: { $path }
result-write-back-summary = 書き戻し: 訳文 { $translated } 単位、原文 { $original } 単位。自動折返し { $auto_wrapped }、改行追加 { $breaks }、全角インデント追加 { $indents }。手動配置 { $manual }
result-generic-extract-unchanged = Generic 入力に変更なし: { $files } ファイル、{ $groups } グループ、{ $units } 単位
result-generic-extract-updated = Generic 入力を更新: { $files } ファイル、{ $groups } グループ、{ $units } 単位。訳文 { $preserved } 件を保持し、{ $cleared } 件を消去
result-generic-translate-summary = Generic 翻訳: タスク { $total }、完全 { $complete }、部分 { $partial }、利用不可 { $unavailable }。クリア { $cleared }、再利用 { $reused }、受理 { $accepted }、書き込み { $written }、競合 { $conflicted }、応答問題 { $problems }
result-generic-write-back-summary = Generic 書き戻し: 訳文 { $translated } 単位、原文保持 { $original } 単位
result-symbol-repair-summary = 記号修復: { $attempted } 単位を確認、{ $repaired } 単位を修復、内部スキップ { $skipped } 単位、{ $replacements } 記号を置換
result-cancelled = 安全な終了処理後にコマンドをキャンセルしました。
result-plan-saved = 成功した実行プランを保存しました。
log-run-started = コマンド { $command } を開始しました。
log-run-succeeded = コマンド { $command } は正常に完了しました。
log-run-failed = コマンド { $command } に失敗しました。
log-run-outcome-unknown = コマンド { $command } は終了しましたが、最終結果は不明です。エラーに示された復旧場所を確認してください。
log-run-cancelled = コマンド { $command } をキャンセルしました。
log-performance-counters = パフォーマンスカウンター：SQLite トランザクション制御の試行 { $sqlite_control_attempted_total } 回、候補ツリー全体の検証開始 { $candidate_validation_started } 回、完了 { $candidate_validation_completed } 回。
log-lua-print = Lua：{ $message }
log-plan-resolved = コマンド { $command } のプラン元: { $source }。
log-phase-started = フェーズ開始: { $phase }。
log-retry-summary = { $count } 回再試行しました。
log-translation-task-started = 翻訳タスク { $index }/{ $total } を開始しました。
log-translation-task-finished = 翻訳タスク { $index } は結果 { $outcome } で終了しました。
log-run-recovery-required = コマンド { $command } は復旧が必要な状態で終了しました。診断に示された復旧場所を確認してください。
log-phase-completed = フェーズ完了: { $phase }。
log-phase-stopped = { $outcome ->
    [failed] フェーズ失敗: { $phase }。
    [cancelled] フェーズをキャンセルしました: { $phase }。
   *[other] フェーズ停止: { $phase }。
}
log-cancellation-requested = { $total } 件中 { $confirmed } 件の確定後にキャンセルが要求されました。
log-cancellation-requested-indeterminate = { $confirmed } 件の確定後にキャンセルが要求されました。総数は不明です。
log-run-plan-finalized = { $result ->
    [saved] 実行計画を保存しました。
    [not_saved] 実行計画は保存されませんでした。
    [saved_finalization_failed] 実行計画は保存されましたが、終了処理に失敗しました。
    [outcome_unknown] 実行計画の最終状態は不明です。
   *[other] 実行計画の終了処理が不明な結果で停止しました。
}
log-translation-finished = { $result ->
    [not_started] 翻訳は開始されませんでした。
    [no_work] 翻訳対象がないため終了しました。
    [complete] 翻訳が完了しました。
    [incomplete] 未完了の作業を残して翻訳が終了しました。
    [failed] 翻訳に失敗しました。
    [cancelled] 翻訳をキャンセルしました。
   *[other] 翻訳が不明な結果で停止しました。
}
log-publication-started = 出力ルート { $path } への公開を開始しました。
log-publication-finished = { $result ->
    [published] 公開が完了しました。
    [not_published] 公開による出力変更はありませんでした。
    [recovery_required] 公開が停止し、復旧が必要です。
    [outcome_unknown] 公開の最終状態は不明です。
   *[other] 公開が不明な結果で停止しました。
}
log-project-log-degraded = プロジェクトログで障害が発生し、{ $failure_kinds } 種類の障害を記録しました。
log-task-outcome-value = { $outcome ->
    [complete] 完了
    [partial] 一部完了
    [unavailable] 利用不可
    [failed] 失敗
    [not_committed_after_earlier_failure] 先行失敗により未コミット
    [cancelled] キャンセル
   *[other] 不明な結果で終了
}
diagnostic-location = 場所：{ $subject }
diagnostic-explanation = 原因：{ $reason }
diagnostic-resolution = 対処：{ $action }
diagnostic-related = 関連エラー { $index }：
diagnostic-resolution-value = { $code ->
    [fix_configuration] 指定された設定項目を修正して再試行してください
    [fix_input] 指定された入力を修正して再試行してください
    [fix_placeholder_rules] 指定された Placeholder ルールを修正して再試行してください
    [adjust_manual_layout] 指定された位置と表示幅に合わせて改行とレイアウトを手動で調整してください
    [check_path_and_permissions] パス、ファイルシステムの状態、権限を確認してください
    [check_project_state] プロジェクトの状態を確認・修正して再試行してください
    [resolve_contention] 競合する操作の完了を待ってから再試行してください
    [check_model_service] モデルサービスの応答とアカウント制限を確認してください
    [preserve_recovery_artifacts] 記載された復旧用ファイルを削除せず、出力を復旧してから再試行してください
    [retry] 操作を再試行してください
    [report_bug] ATT の不具合を報告し、そのとき行っていた操作を説明してください
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 必須値がありません
    [generic_extract_required] JSONL 入力が直近の Extract と一致しません。att generic extract を再実行してください
    [conflicting_values] 指定された値が競合しています
    [invalid_syntax] 値の構文が無効です
    [invalid_encoding] テキストのエンコーディングが無効です
    [invalid_value] 値が必要な契約に違反しています
    [not_found] 必要な対象が存在しません
    [state_mismatch] 保存されたプロジェクト状態がこの操作の要件を満たしていません
    [unsupported_windows_code_page] Windows のコードページが UTF-8 ではありません
    [transaction_rolled_back] トランザクションが失敗し、変更はロールバックされました
    [transaction_outcome_unknown] トランザクションのコミットまたはロールバックを確認できませんでした
    [finalization_failed] 操作結果は存在しますが、確定処理に失敗しました
    [rollback_failed] 主操作に失敗し、ロールバックにも失敗しました
    [external_service_rejected] 外部サービスがリクエストを拒否しました
    [external_service_unavailable] 外部サービスを利用できません
    [executor_closed] 実行サービスは終了中、またはすでに終了しています
    [concurrent_shutdown] 別の呼び出し元が実行器を終了しています
    [executor_state_poisoned] 実行器のライフサイクル状態が破損しています
    [worker_spawn_failed] オペレーティングシステムがワーカースレッドを作成できませんでした
    [worker_channel_closed] 確定処理の完了前にワーカーのコマンドチャネルが閉じました
    [worker_panicked] ワーカーが予期せず終了しました
    [reparse_point_forbidden] パスに信頼できない再解析ポイントが含まれています
    [non_local_volume] パスがローカル固定ボリューム上にありません
    [non_ntfs_volume] パスが NTFS ボリューム上にありません
    [case_sensitive_directory] ディレクトリで大文字と小文字を区別する名前規則が有効です
    [lock_cancelled] 必要なロックの待機がキャンセルされました
    [target_already_exists] 出力先がすでに存在します
    [file_identity_changed] 操作中にファイルの識別情報が変化しました
    [invalid_path] パスはこの操作の有効な対象ではありません
    [wrong_publisher_instance] 公開トークンは別の公開器インスタンスに属しています
    [journal_corrupt] 公開復旧ジャーナルが無効または不完全です
    [unexpected_artifact] 予期しないファイルシステム成果物が操作を妨げています
    [interactive_session_already_open] 別の対話型 SQLite セッションがすでに実行中です
    [backup_incomplete] SQLite バックアップが完了状態に達しませんでした
    [request_serialization_failed] モデルリクエストをシリアル化できませんでした
    [response_parsing_failed] モデル応答が有効な JSON ではありません
    [invalid_response_contract] モデル応答が必要な応答契約を満たしていません
    [transport_failed] 有効な応答を受け取る前に HTTP 転送が失敗しました
    [lua_compilation_failed] Lua メインプログラムをコンパイルできませんでした
    [lua_execution_failed] Lua メインプログラムの実行中に失敗しました
    [rules_pattern_match_failed] Rules の PCRE2 パターンを評価できませんでした
    [rules_zero_width_match] Rules パターンがゼロ幅一致を生成しました
    [rules_overlapping_capture] Rules パターンが重複するテキストキャプチャを生成しました
    [rules_missing_text_capture] 必須の名前付きテキストキャプチャが一致に参加しませんでした
    [rules_invalid_capture_range] Rules の一致またはキャプチャ範囲が有効な UTF-8 文字境界外です
    [write_back_candidate_invalid] 書き戻し候補が必要な data/js ツリー構造を満たしていません
    [write_back_recovery_required] 内容を信頼する前に出力ディレクトリの復旧が必要です
    [already_exists] 対象オブジェクトは既に存在します
    [cancelled] 操作はキャンセルされました
    [concurrent_modification] 操作中にプロジェクト状態が同時変更されました
    [duplicate_identifier] 識別子が重複しています
    [extraction_out_of_date] 保存済みの抽出結果は現在のソースと一致しません
    [invalid_content] 内容が必須契約に違反しています
    [manual_layout_required] 改行またはレイアウトの手動調整が必要です
    [operation_failed] 操作に失敗しました
    [placeholder_projection_failed] Placeholder の投影で必須構造が保持されませんでした
    [profile_not_found] 選択した翻訳 Profile は存在しません
    [recovery_required] 結果を信頼する前に復旧が必要です
    [resource_limit] 必要なリソース上限に達しました
    [resource_limit_exceeded] 操作がバックエンドのリソース上限を超えました
    [source_snapshot_mismatch] ソースは保存済みスナップショットと一致しません
    [unavailable] 要求された作業は一時的に利用できません
    [internal_invariant] 内部不変条件に違反しました。ATT の不具合です
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] 言語ポリシー用語を空白にできません
    [language_policy_term_surrounding_whitespace] 言語ポリシー用語の前後に空白を含められません
    [language_policy_term_duplicate] 言語ポリシー用語を重複させられません
    [language_id_blank] 言語 ID を空白にできません
    [language_id_surrounding_whitespace] 言語 ID の前後に空白を含められません
    [language_id_uses_underscore] 言語 ID のサブタグ間にはハイフンを使用してください
    [language_id_invalid_syntax] 言語 ID は RFC 5646 構文を満たす必要があります
    [language_id_invalid_registry_tag] 言語 ID に無効な登録済みサブタグが含まれています
    [language_id_canonicalization_failed] 言語 ID を正規化できません
    [language_id_undefined_primary_language] 言語 ID に第一言語が必要です
    [language_id_duplicate] 言語 ID は一意でなければなりません
    [language_catalog_empty] ソース言語モジュールが 1 つ以上必要です
    [url_invalid] 値は有効な URL でなければなりません
    [url_credentials_forbidden] URL に認証情報を含められません
    [url_fragment_forbidden] URL にフラグメントを含められません
    [url_scheme_unsupported] URL スキームは http または https でなければなりません
    [api_key_blank] API key を空白にできません
    [api_key_surrounding_whitespace] API key の前後に空白を含められません
    [api_key_invalid_header] API key を HTTP Header 値として表現できません
    [strict_json_invalid] 値は厳密な JSON でなければなりません（行={ $line }、列={ $column }）
    [json_object_required] 値は JSON オブジェクトでなければなりません
    [reserved_request_field] このフィールドはリクエストプロトコルが所有しているため上書きできません
    [proxy_must_be_false_or_url] proxy は false または完全な http/https URL でなければなりません
    [pem_path_duplicate] PEM パスは一意でなければなりません
    [runtime_maximum_exceeded] 値がランタイム上限を超えています（実際={ $actual }、上限={ $maximum }）
    [value_surrounding_whitespace] 値の前後に空白を含められません
    [value_blank] 値を空白にできません
    [path_blank] パスを空にできません
    [positive_required] 値は 0 より大きい必要があります（実際={ $actual }）
    [usize_range_exceeded] 値がこのプラットフォームの usize 範囲を超えています（実際={ $actual }）
    [u32_range_exceeded] 値が u32 範囲を超えています（実際={ $actual }）
    [duplicate_profile_id] 翻訳プロファイル ID は一意でなければなりません
    [selected_profile_invalid] 選択した翻訳プロファイルの構造またはフィールド型が無効です
    [referenced_client_not_found] 参照された LLM クライアントが存在しません
   *[other] __ATT_FALLBACK__
}
diagnostic-http-status = HTTP ステータス { $status }
diagnostic-retry-after = Retry-After：{ $seconds } 秒
diagnostic-provider-code = プロバイダー code：{ $code }
diagnostic-provider-type = プロバイダー type：{ $kind }
diagnostic-provider-message = プロバイダーのメッセージ：{ $message }
diagnostic-json-position = { $line } 行、{ $column } 列
diagnostic-placeholder-rule-file = { $path } の Placeholder ルール { $number }
diagnostic-placeholder-rule-project = 現在のプロジェクトの Placeholder ルール { $number }
manual-exported = { $entries } 件を { $path } にエクスポートしました
manual-checked = 有効 { $valid }、未入力 { $unfilled }、エラー { $errors }
manual-applied = 適用 { $applied }、未入力 { $unfilled }、エラー { $errors }
manual-issue = { $object }：{ $reason }。{ $help }。
manual-value = { $code ->
    [invalid_source_line] source の { $line } 番目に改行または NUL が含まれています
    [invalid_translation_line] translation の { $line } 番目に改行または NUL が含まれています
    [fixed_length] fixed 訳には { $expected } 項必要ですが、{ $actual } 項あります
    [fixed_blank_slot] fixed 訳の { $line } 番目は空のままにしてください
    [rerun_export] manual export を再実行してください
    [rerun_export_without_controls] manual export を再実行し、配列項目に改行や NUL を入れないでください
    [rerun_export_then_fill] manual export を再実行してから訳文を入力してください
    [keep_exported_type] manual export が出力した type を保持してください
   *[other] __ATT_FALLBACK__
}
task-record-title = 翻訳タスク { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] 完了
    [partial] 一部完了
    [unavailable] 利用不可
    [execution_failed] 実行失敗
    [commit_preparation_failed] コミット準備失敗
    [commit_not_applied] コミット未適用
    [commit_outcome_unknown] コミット結果不明
    [not_committed_after_earlier_failure] 先行失敗により未コミット
    [invalid_result] Executor 結果列が無効
    [cancelled] キャンセル済み
   *[other] { $state }
}
task-record-summary-with-written = `タスク { $ordinal }/{ $total }` · `試行 { $attempts } 回` · `検収 { $accepted }/{ $expected }` · `書き込み { $written } 箇所`
task-record-summary-without-written = `タスク { $ordinal }/{ $total }` · `試行 { $attempts } 回` · `検収 { $accepted }/{ $expected }`
task-record-run-id-label = Run ID：
task-record-started-at-label = 開始時刻：
task-record-duration-label = 合計時間：
task-record-endpoint-label = Endpoint：
task-record-model-label = Model：
task-record-custom-parameters-heading = カスタムパラメーター
task-record-attempts-heading = リクエスト経過
task-record-final-result-heading = 最終結果
task-record-no-request = 送信可能なモデルリクエストは作成されませんでした。
task-record-empty-assistant = モデルは空のオブジェクトを返しました。
task-record-parse-error = 解析エラー：{ $kind ->
    [thinking_empty] 思考内容が空です（{ $line } 行 { $column } 列）
   *[json] モデル応答の JSON が無効です（カテゴリ `{ $category }`、{ $line } 行 { $column } 列）
}
task-record-attempt-succeeded = 試行 { $number }：成功；finish reason { $finish_reason }
task-record-attempt-token-usage = ；token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ；所要時間 `{ $duration }`
task-record-attempt-retryable = 試行 { $number }：再試行可能なリクエスト失敗；所要時間 `{ $duration }`
task-record-attempt-retry-after = ；Retry-After `{ $duration }`
task-record-attempt-wait-retry = ；`{ $duration }` 後に再試行
task-record-attempt-wait-completed = ；`{ $duration }` の待機は完了しましたが、次の試行は開始されませんでした
task-record-attempt-wait-cancelled = ；`{ $duration }` の待機中にキャンセル
task-record-attempt-failed = 試行 { $number }：リクエストまたはレスポンス処理失敗；所要時間 `{ $duration }`
task-record-attempt-cancelled = 試行 { $number }：キャンセル済み；所要時間 `{ $duration }`
task-record-final-status = 状態：{ $state ->
    [complete] 完了、コミット確認済み
    [partial] 一部完了、コミット確認済み
    [unavailable] 利用不可、プロジェクト変更なし
    [execution_failed] 実行失敗、未コミット
    [commit_preparation_failed] コミット準備失敗、未適用を確認
    [commit_not_applied] トランザクション未適用を確認
    [commit_outcome_unknown] コミット結果不明
    [not_committed_after_earlier_failure] 先行タスク失敗により未コミット
    [invalid_result] Executor 結果列が無効、未コミット
    [cancelled] キャンセル済み、未コミット
   *[other] { $state }
}
task-record-accepted-written = 受理：{ $accepted } 項目、実位置 { $written } 箇所へ書き込み
task-record-accepted-outcome-unknown = 検収済み：{ $accepted } 項目；データベースのコミット結果を確認できません
task-record-task-diagnostic = タスク診断
task-record-duration-seconds = { $value } 秒
task-record-duration-milliseconds = { $value } ミリ秒
