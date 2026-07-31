app-about = 再利用可能なプロジェクト状態でゲームと構造化テキストを翻訳します
cli-config-help = 今回の実行で使用する厳密な TOML 設定ファイル
cli-ui-language-help = ヘルプ、診断、進捗、結果、プロジェクトログの言語: ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko、vi
cli-progress-help = 進捗表示モード: auto、plain、off
cli-mz-about = RPG Maker MZ ゲームを翻訳します
cli-mv-about = RPG Maker MV ゲームを翻訳します
cli-generic-about = 規定の JSONL テキストを翻訳します
cli-init-about = 名前付き翻訳プロジェクトを初期化または更新します
cli-extract-about = プロジェクトの現在の入力から原文を同期します
cli-translate-about = 明示または保存済み Profile で抽出済み原文を翻訳します
cli-write-back-about = 現在の訳文をプロジェクトの出力へ書き込みます
cli-project-lua-about = プロジェクトで原子的なデータベース Lua を一度実行します
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
cli-project-lua-script-help = 一度だけ実行する原子的なデータベース Lua プログラム
cli-project-lua-arguments-help = -- の後で Lua arg[1..] に渡す UTF-8 引数
cli-usage-heading = 使用法:
cli-commands-heading = コマンド:
cli-options-heading = オプション:
cli-arguments-heading = 引数:
cli-options-metavar = オプション
cli-command-metavar = コマンド
cli-print-help = ヘルプを表示します
cli-print-version = バージョンを表示します
cli-missing-config = 必須の設定パス --config <FILE> がありません。
cli-blank-value = 値を空にすることはできません。
cli-invalid-positive-integer = 値は正の整数でなければなりません。
cli-invalid-progress = 進捗モード { $value } は未対応です。auto、plain、off のいずれかを使用してください。
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
log-label-phase-check-project = プロジェクト確認
log-label-phase-scan-source = ソース走査
log-label-phase-prepare-candidate = 候補準備
log-label-phase-update-database = データベース更新
log-label-phase-publish = 公開
log-label-phase-builtin = 組み込み抽出
log-label-phase-rules = ルール抽出
log-label-phase-lua = Lua 処理
log-label-phase-planning = 計画
log-label-phase-confirmed-tasks = タスク確認
log-label-phase-no-work = 作業不要
log-label-phase-read-assets = アセット読み取り
log-label-phase-plan-rpg-maker-write-back = RPG Maker 書き戻し計画
log-label-phase-rewrite-documents = ドキュメント書き換え
log-label-phase-validate-candidate = 候補検証
log-label-task-complete = 完了
log-label-task-partial = 一部利用可能
log-label-task-unavailable = 利用不可
log-label-task-failed = 失敗
error-state-applied-finalization = 結果は反映されましたが、終了処理に失敗しました。再試行前にプロジェクト状態を確認してください。
error-no-executable-extract-owner = 消去後に実行可能な Extract owner がないため、プランは保存されませんでした。
error-plan-save-failed-applied = コマンド結果は反映されましたが、新しい実行プランは保存されませんでした。次回は意図したオプションを明示してください。
error-plan-save-outcome-unknown = コマンド結果は反映されましたが、実行プランのコミット結果を確認できません。次回は意図したオプションを明示してください。
plan-source-explicit = 明示入力
plan-source-project-state = プロジェクト状態
plan-source-product-default = 製品動作
notice-init-reuse-path = 元パスが指定されなかったため、前回成功したパスを再利用します: { $path }。
notice-extract-reuse-owners = 抽出範囲が指定されなかったため、前回成功したプランを再利用します: { $owners }。
notice-translate-reuse-profile = Profile が指定されなかったため、前回成功した Profile を再利用します: { $profile }。
notice-owner-disabled = owner { $owner } を無効にし、今後の自動プランから削除しました。
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
result-cancelled = 安全な終了処理後にコマンドをキャンセルしました。
result-plan-saved = 成功した実行プランを保存しました。
log-run-started = コマンド { $command } を開始しました。
log-run-succeeded = コマンド { $command } は正常に完了しました。
log-run-failed = コマンド { $command } に失敗しました。
log-run-outcome-unknown = コマンド { $command } は終了しましたが、最終結果は不明です。エラーに示された復旧場所を確認してください。
log-run-cancelled = コマンド { $command } をキャンセルしました。
log-performance-counters = パフォーマンスカウンター：SQLite トランザクション制御の試行 { $sqlite_control_attempted_total } 回、候補ツリー全体の検証開始 { $candidate_validation_started } 回、完了 { $candidate_validation_completed } 回。
log-lua-script = Lua スクリプト { $identity }（SHA-256 { $fingerprint }）。
log-lua-print = Lua：{ $message }
log-lua-summary = Lua をコミットしました：データベース呼び出し { $database_calls } 回、変更行 { $changed_rows } 行、翻訳呼び出し { $translation_calls } 回、print { $printed_lines } 行。
log-plan-resolved = コマンド { $command } のプラン元: { $source }。
log-phase-started = フェーズ開始: { $phase }。
log-phase-finished = フェーズ完了: { $phase }。
log-retry-summary = { $count } 回再試行しました。
log-no-work = 作業は不要でした: { $reason }。
log-no-work-translation-up-to-date = 翻訳は現在のソースとプロファイルに一致しています
log-partial-result = 注意が必要な部分結果が { $count } 件あります。
log-translation-task-started = 翻訳タスク { $index }/{ $total } を開始しました。
log-translation-task-finished = 翻訳タスク { $index } は結果 { $outcome } で終了しました。
log-translation-task-diagnostic = 翻訳タスク { $index } は { $attempts } 回の試行後に診断を報告しました: { $diagnostic }
diagnostic-title = エラー [{ $code }]
diagnostic-stage = 段階：{ $stage }
diagnostic-subject = 場所：{ $subject }
diagnostic-subject-value = { $kind ->
    [command] コマンド { $value }
    [field] フィールド { $value }
    [project] プロジェクト { $value }
    [profile] プロファイル { $value }
    [component] コンポーネント { $value }
   *[other] { $value }
}
diagnostic-reason = 原因：{ $reason }
diagnostic-impact = 影響：{ $impact }
diagnostic-action = 対処：{ $action }
diagnostic-recovery = 復旧場所：{ $recovery }
diagnostic-recovery-value = { $kind ->
    [component] コンポーネント { $value }
    [transaction] トランザクション { $value }
   *[other] { $value }
}
diagnostic-related = 関連エラー { $index }：
diagnostic-stage-value = { $code ->
    [process_startup] プロセス起動
    [process_output] プロセス出力
    [configuration] 設定の読み込み
    [command_preparation] コマンドの準備
    [project_opening] プロジェクトを開く処理
    [init] 初期化
    [extract] 抽出
    [translate] 翻訳
    [write_back] 書き戻し
    [lua] プロジェクト Lua 実行
    [model_request] モデルへのリクエスト
    [run_plan_finalization] 実行プランの確定
    [publication] 公開
    [shutdown] 終了処理
    [logging] プロジェクトログ
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] 状態は変更されていません
    [valid_progress_preserved] 有効な進捗は保存されました
    [result_applied_but_run_plan_not_saved] 結果は適用されましたが、実行プランは保存されませんでした
    [state_applied_but_finalization_failed] 状態は適用されましたが、確定処理は完了しませんでした
    [recovery_required] 状態を信頼する前に復旧が必要です
    [outcome_unknown] 最終状態は不明です
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] 指定された設定項目を修正して再試行してください
    [fix_input] 指定された入力を修正して再試行してください
    [check_path_and_permissions] パス、ファイルシステムの状態、権限を確認してください
    [check_project_state] プロジェクトの状態を確認・修正して再試行してください
    [retry_after_resolving_contention] 競合する操作の完了を待ってから再試行してください
    [check_model_service] モデルサービスの応答とアカウント制限を確認してください
    [preserve_recovery_artifacts] 記載された復旧用ファイルを削除せず、出力を復旧してから再試行してください
    [retry] 操作を再試行してください
    [report_bug] エラーコードとログパスを添えて ATT の不具合を報告してください
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 必須値がありません
    [extract_plan_required] 再利用可能な Extract プランが保存されていません。--builtin または --rules を指定してください
    [generic_extract_required] JSONL 入力が直近の Extract と一致しません。att generic extract を再実行してください
    [conflicting_values] 指定された値が競合しています
    [invalid_syntax] 値の構文が無効です
    [invalid_encoding] テキストのエンコーディングが無効です
    [invalid_value] 値が必要な契約に違反しています
    [not_found] 必要な対象が存在しません
    [busy] リソースは別の操作によって使用中です
    [state_mismatch] 保存されたプロジェクト状態がこの操作の要件を満たしていません
    [requirement_failed] 必要な前提条件が満たされていません
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
    [lua_database_open_failed] Lua ホストがプロジェクトデータベースのセッションを開けませんでした
    [lua_context_creation_failed] Lua ランタイムが VM コンテキストを作成できませんでした
    [lua_compilation_failed] Lua メインプログラムをコンパイルできませんでした
    [lua_execution_failed] Lua メインプログラムの実行中に失敗しました
    [lua_host_call_failed] Lua ホスト機能の呼び出しに失敗しました
    [lua_finalization_failed] Lua ホストがすべてのバインド済みリソースを確定できませんでした
    [rules_definition_invalid] Rules プログラムが Rules 定義契約を満たしていません
    [rules_document_read_failed] Rules プログラムに必要なソース文書を読み取れませんでした
    [rules_no_non_blank_match] Rules エントリから空白以外の意味単位が生成されませんでした
    [rules_invalid_target] Rules エントリがテキスト対象として使用できない値を選択しました
    [rules_pattern_match_failed] Rules の PCRE2 パターンを評価できませんでした
    [rules_zero_width_match] Rules パターンがゼロ幅一致を生成しました
    [rules_overlapping_capture] Rules パターンが重複するテキストキャプチャを生成しました
    [rules_missing_text_capture] 必須の名前付きテキストキャプチャが一致に参加しませんでした
    [rules_invalid_capture_range] Rules の一致またはキャプチャ範囲が有効な UTF-8 文字境界外です
    [rules_duplicate_target] 2 つの Rules エントリが同じ物理テキスト対象を要求しています
    [rules_invalid_materialization] Rules の投影レシピでソース値を再構築できません
    [rules_snapshot_invalid] 抽出された Rules グループが有効なアセットスナップショットを形成しません
    [rules_snapshot_store_failed] 検証済み Rules 抽出スナップショットをコミットできませんでした
    [write_back_extraction_out_of_date] 抽出済みアセットが現在のプロジェクトソースと一致しません
    [write_back_asset_snapshot_invalid] 保存された RPG Maker アセットが有効な書き戻しスナップショットを形成しません
    [source_document_invalid] RPG Maker のソース文書が必要な文書形式を満たしていません
    [generic_source_document_invalid] Generic JSONL のソース文書が必要な形式を満たしていません
    [write_back_mutation_invalid] 検証済み翻訳変更を固定されたソース位置に適用できません
    [write_back_output_path_invalid] 書き換えたファイルが許可された RPG Maker 出力ツリー外にあります
    [write_back_output_path_duplicate] 複数の書き換えファイルが同じ出力パスを対象にしています
    [write_back_candidate_project_mismatch] 準備済み書き戻し候補は別のプロジェクトに属しています
    [write_back_candidate_invalid] 書き戻し候補が必要な data/js ツリー構造を満たしていません
    [write_back_not_published] 書き戻し候補が現在の出力ディレクトリを置き換えませんでした
    [write_back_published_with_residuals] 出力は公開されましたが、一部の復旧成果物を削除できませんでした
    [write_back_recovery_required] 内容を信頼する前に出力ディレクトリの復旧が必要です
    [internal_invariant] 内部不変条件に違反しました。ATT の不具合です
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] 見つかりません
    [permission_denied] 権限がありません
    [connection_refused] 接続が拒否されました
    [connection_reset] 接続がリセットされました
    [host_unreachable] ホストに到達できません
    [network_unreachable] ネットワークに到達できません
    [connection_aborted] 接続が中止されました
    [not_connected] 接続されていません
    [address_in_use] アドレスは使用中です
    [address_not_available] アドレスを使用できません
    [network_down] ネットワークが停止しています
    [broken_pipe] パイプが切断されています
    [already_exists] すでに存在します
    [would_block] 操作はブロックされます
    [not_a_directory] ディレクトリではありません
    [is_a_directory] ディレクトリです
    [directory_not_empty] ディレクトリが空ではありません
    [read_only_filesystem] 読み取り専用ファイルシステムです
    [stale_network_file_handle] ネットワークファイルハンドルが失効しています
    [invalid_input] 操作入力が無効です
    [invalid_data] データが無効です
    [timed_out] 操作がタイムアウトしました
    [write_zero] 書き込みが進行しませんでした
    [storage_full] ストレージがいっぱいです
    [not_seekable] 対象をシークできません
    [quota_exceeded] ストレージ割り当てを超過しました
    [file_too_large] ファイルが基盤システムで扱えるサイズを超えています
    [resource_busy] リソースは使用中です
    [executable_file_busy] 実行可能ファイルは使用中です
    [deadlock] 操作がデッドロックを引き起こします
    [crosses_devices] 操作がファイルシステムデバイスをまたいでいます
    [too_many_links] ファイルシステムリンクが多すぎます
    [invalid_filename] ファイル名が無効です
    [argument_list_too_long] OS の引数リストが長すぎます
    [interrupted] 操作が中断されました
    [unsupported] 操作はサポートされていません
    [unexpected_eof] 予期しないファイル終端です
    [out_of_memory] OS がメモリを割り当てられませんでした
    [other] その他の OS エラー
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [runtime_configuration_invalid] ランタイム設定が無効です
    [unsupported_prompt_locale] 小文字の auto またはサポートされている BCP 47 UI ロケールでなければなりません
    [language_policy_term_blank] 言語ポリシー用語を空白にできません
    [language_policy_term_surrounding_whitespace] 言語ポリシー用語の前後に空白を含められません
    [language_policy_term_duplicate] 言語ポリシー用語を重複させられません
    [quote_repair_candidates_empty] 引用符修復候補リストを空にできません
    [quote_repair_delimiter_invalid] 引用符修復の区切り文字に英数字、空白、制御文字は使用できません
    [quote_repair_pair_duplicate] 引用符修復ペアを重複させられません
    [quote_repair_delimiter_ambiguous] 引用符修復の区切り文字は 1 つのペアだけに属する必要があります
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
diagnostic-io-reason = 操作 { $operation }：{ $kind }
diagnostic-io-reason-with-os-code = 操作 { $operation }：{ $kind }（OS { $os_code }）
diagnostic-io-reason-with-system-message = 操作 { $operation }：{ $kind }：{ $system_message }
diagnostic-io-reason-with-os-code-and-system-message = 操作 { $operation }：{ $kind }（OS { $os_code }）：{ $system_message }
diagnostic-failure-with-detail = { $failure }：{ $detail }
diagnostic-invalid-utf8 = バイト { $valid_up_to } の UTF-8 が無効です。無効な長さは { $error_len } バイトです
diagnostic-incomplete-utf8 = バイト { $valid_up_to } の後に未完了の UTF-8 シーケンスがあります
diagnostic-toml-failure-value = { $code ->
    [syntax] TOML 構文が無効です
    [missing_field] 必須の設定フィールドがありません
    [unknown_field] 設定に不明なフィールドがあります
    [duplicate_field] 設定フィールドが複数回宣言されています
    [type_mismatch] { $expected }が必要です
    [invalid_value] 設定値がフィールド契約に違反しています
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] 文字列
    [integer] 整数
    [boolean] 真偽値
    [string_or_boolean] 文字列または真偽値
    [string_array] 文字列の配列
    [integer_array] 整数の配列
    [string_pair_array] 文字列ペアの配列
    [table] テーブル
    [table_array] テーブルの配列
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = TOML が無効です（{ $resource }）：{ $failure }
diagnostic-invalid-toml-at = { $line } 行 { $column } 列の TOML が無効です（{ $resource }）：{ $failure }
diagnostic-http-no-details = モデルサービスへのリクエストは失敗しましたが、公開可能な HTTP 状態の詳細はありません
diagnostic-http-status = HTTP ステータス { $status }
diagnostic-http-retry-after = Retry-After { $seconds } 秒
diagnostic-http-provider-code = プロバイダーエラーコード { $code }
diagnostic-http-provider-type = プロバイダーエラー種別 { $kind }
diagnostic-http-fact-separator = ；
diagnostic-sqlite = SQLite 主エラーコード { $primary_code }、拡張エラーコード { $extended_code }
diagnostic-windows-status = Windows 操作 { $operation } が失敗しました。NTSTATUS { $status }
diagnostic-resource = { $resource }：実際値 { $actual }
diagnostic-resource-with-maximum = { $resource }：実際値 { $actual }、上限 { $maximum }
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
    [json] モデル応答の JSON が無効です（カテゴリ `{ $category }`、{ $line } 行 { $column } 列）
    [thinking_not_allowed] 現在の応答モードでは思考出力を受け付けません（{ $line } 行 { $column } 列）
    [thinking_envelope_missing] 必須の思考エンベロープがありません（{ $line } 行 { $column } 列）
    [thinking_envelope_unclosed] 思考エンベロープが閉じられていません（{ $line } 行 { $column } 列）
    [thinking_empty] 思考内容が空です（{ $line } 行 { $column } 列）
    [thinking_nested] 入れ子の思考エンベロープがあります（{ $line } 行 { $column } 列）
    [thinking_repeated] 思考エンベロープが重複しています（{ $line } 行 { $column } 列）
    [markdown_fence_no_body] Markdown フェンスに本文がありません（{ $line } 行 { $column } 列）
    [markdown_fence_unsupported] 言語指定なし、または json 指定の単一 Markdown フェンスだけを受け付けます（{ $line } 行 { $column } 列）
    [markdown_fence_unclosed] Markdown フェンスが閉じられていません（{ $line } 行 { $column } 列）
   *[markdown_fence_invalid_closing] Markdown フェンスは末尾の独立行で閉じる必要があります（{ $line } 行 { $column } 列）
}
task-record-attempt-succeeded = 試行 { $number }：成功；finish reason { $finish_reason }
task-record-attempt-token-usage = ；token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ；所要時間 `{ $duration }`
task-record-attempt-request-id = ；request ID { $request_id }
task-record-attempt-response-id = ；response ID { $response_id }
task-record-attempt-retryable = 試行 { $number }：再試行可能なリクエスト失敗；診断 `{ $code }`；所要時間 `{ $duration }`
task-record-attempt-retry-after = ；Retry-After `{ $duration }`
task-record-attempt-wait-retry = ；`{ $duration }` 後に再試行
task-record-attempt-wait-completed = ；`{ $duration }` の待機は完了しましたが、次の試行は開始されませんでした
task-record-attempt-wait-cancelled = ；`{ $duration }` の待機中にキャンセル
task-record-attempt-failed = 試行 { $number }：リクエストまたはレスポンス処理失敗；診断 `{ $code }`；所要時間 `{ $duration }`
task-record-attempt-cancelled = 試行 { $number }：キャンセル済み；所要時間 `{ $duration }`
task-record-structured-reason = 理由：{ $reason }
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
task-record-rejected-heading = 未受理：
task-record-rejected-item = { $id }：{ $reason }
task-record-protocol-diagnostic = プロトコル診断：{ $diagnostic }
task-record-unavailable-reason = 利用不可の理由：{ $reason }
task-record-task-diagnostic = タスク診断：`{ $code }`；理由 { $reason }
task-record-rejection-reason = { $code ->
    [missing] モデル出力がありません
    [duplicate] モデル出力が重複しています
    [invalid_shape] { $detail }
    [invalid_shape_array] 翻訳は文字列配列である必要があります
    [invalid_shape_item] 翻訳配列の { $line } 番目の項目は文字列である必要があります
    [line_count_mismatch] 行数不一致（期待 { $expected }、実際 { $actual }）
    [invalid_line_text] { $line } 行目に無効な制御文字があります
    [blank_line_mismatch] { $line } 行目の空白状態不一致（期待：{ $expected_blank ->
        [blank] 空白
       *[other] 非空白
    }）
    [blank_translation] 翻訳が空です
    [no_natural_language_text] 翻訳に自然言語テキストがありません
    [contains_byte_order_mark] 翻訳に BOM が含まれます
    [placeholder_mismatch] プレースホルダー不一致：{ $detail }
    [unexpected_placeholder] 未知のプレースホルダー：{ $detail }
    [placeholder_normalization_ambiguous] プレースホルダーの正規化が曖昧です：{ $detail }
    [source_residual] 原文言語の残留を検出：{ $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason が stop ではありません：{ $detail }
    [invalid_response] { $detail }
    [invalid_id] モデルの { $index } 番目の項目の ID が無効です
    [unknown_id] モデルの { $index } 番目の項目が未知の ID { $detail } を返しました
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] モデルレスポンスを解析できません
    [all_outputs_rejected] すべてのモデル出力が検収で拒否されました
    [recoverable_request_exhausted] 回復可能なリクエストの再試行回数を使い切りました
    [retry_after_exceeds_maximum] Retry-After が設定済み最大待機時間を超えています
   *[other] { $code }
}
task-record-duration-seconds = { $value } 秒
task-record-duration-milliseconds = { $value } ミリ秒
