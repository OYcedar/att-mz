app-about = 再利用可能なプロジェクト状態でゲームと構造化テキストを翻訳します
cli-test-about = 配布設定とすべての LLM Client を確認します
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
cli-ownership-export-about = 抽出したすべての RPG Maker ユニットのテキスト所有権を出力します
cli-translation-export-about = 抽出したすべてのユニットの原文、現在の翻訳、状態を出力します
cli-manual-check-about = プロジェクトを変更せず手動翻訳 TOML を検査します
cli-manual-apply-about = 入力済みで有効な手動翻訳を適用します
cli-project-lua-about = プロジェクトデータベースに対して Lua スクリプトを実行します
cli-project-name-help = 安定したプロジェクト名
cli-init-path-help = 入力ルートディレクトリ。既存プロジェクトでは前回成功時のパスを再利用できます
cli-source-language-help = 原文の言語 ID
cli-target-language-help = 翻訳先の言語 ID
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
plan-source-explicit = 明示入力
plan-source-project-state = プロジェクト状態
plan-source-product-default = 製品動作
notice-init-reuse-path = 元パスが指定されなかったため、前回成功したパスを再利用します: { $path }。
notice-extract-reuse-owners = 抽出範囲が指定されなかったため、前回成功したプランを再利用します: { $owners }。
notice-translate-reuse-profile = Profile が指定されなかったため、前回成功した Profile を再利用します: { $profile }。
notice-no-model-request = すべての翻訳単位が最新のため、今回はモデルへのリクエストを行いませんでした。
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
progress-no-work = 処理は不要です
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
result-translate-completed = 翻訳処理終了: { $project }（Profile: { $profile }）
result-translate-status = 状態：{ $status }
result-translate-status-value = { $status ->
    [no_work] 処理不要
    [complete] 完了
    [incomplete] 未完了
   *[other] __ATT_FALLBACK__
}
result-translate-summary = 翻訳: 計画 { $total } タスク、開始 { $started }、未開始 { $not_started }、完全 { $complete }、部分 { $partial }、利用不可 { $unavailable }、失敗 { $failed }、取消 { $cancelled }。{ $written } 箇所を書き込み、残り { $remaining } 箇所（Rejected { $rejected } 箇所）
result-translate-convergence = 状態収束: 保持 { $retained }、無効化 { $invalidated }、非該当 { $not_applicable }、再利用 { $reused }
result-write-back-completed = 書き戻し完了: { $project }
result-project-lua-completed = プロジェクト Lua 実行完了: { $project }
result-output-directory = 出力ディレクトリ: { $path }
result-write-back-summary = 書き戻し: 訳文 { $translated } 単位、原文 { $original } 単位
result-generic-extract-unchanged = Generic 入力に変更なし: { $files } ファイル、{ $groups } グループ、{ $units } 単位
result-generic-extract-updated = Generic 入力を更新: { $files } ファイル、{ $groups } グループ、{ $units } 単位。訳文 { $preserved } 件を保持し、{ $cleared } 件を消去
result-generic-translate-summary = Generic 翻訳: 計画 { $total } タスク、開始 { $started }、未開始 { $not_started }、完全 { $complete }、部分 { $partial }、利用不可 { $unavailable }、失敗 { $failed }、取消 { $cancelled }。計画 Unit { $planned_units }、残り Unit { $remaining_units }（Rejected { $rejected_units }）、クリア { $cleared }、再利用 { $reused }、受理 { $accepted }、書き込み { $written }、競合 { $conflicted }、応答問題 { $problems }
result-generic-write-back-summary = Generic 書き戻し: 訳文 { $translated } 単位、原文保持 { $original } 単位
result-run-log = 実行記録：{ $path }
result-test-configuration = 設定：{ $status ->
    [passed] 成功
   *[failed] 失敗
}
result-test-client = LLM { $client }：{ $status ->
    [passed] 成功
   *[failed] 失敗
}（{ $protocol }、{ $stream ->
    [streaming] ストリーミング
   *[non_streaming] 非ストリーミング
}）
result-test-summary = 集計：{ $passed }/{ $total } 成功、{ $failed } 失敗、{ $skipped } 未実行
translate-incomplete-object = プロジェクト { $project } の今回の Translate
translate-incomplete-rpg-maker-reason = 部分タスク { $partial }、利用不可タスク { $unavailable }、未開始タスク { $not_started }、プロトコル問題 { $protocol }、要求枯渇 { $exhausted }。要求受付は{
    $admission ->
        [stopped] 停止
       *[open] 継続
    }。残りの判断 { $remaining_decisions }、残りの場所 { $remaining_locations }（Rejected { $rejected_locations }）
translate-incomplete-generic-reason = 部分タスク { $partial }、利用不可タスク { $unavailable }、未開始タスク { $not_started }、要求枯渇 { $exhausted }。要求受付は{
    $admission ->
        [stopped] 停止
       *[open] 継続
    }。残り Unit { $remaining_units }（Rejected { $rejected_units }）、書き込み競合 { $conflicted }、応答問題 { $problems }
translate-incomplete-help = 今回の実行記録にあるタスク診断を確認し、再現する問題を修正して Translate を再実行してください。少量の残りには Manual を使用できます
translate-incomplete-rejected-help = タスク診断を確認してください。Rejected は --retry-rejected で再翻訳するか、manual export --selection rejected で出力して Manual で処理できます
result-cancelled = 安全な終了処理後にコマンドをキャンセルしました。
result-plan-saved = 成功した実行プランを保存しました。
log-run-started = コマンド { $command } を開始しました。
log-run-succeeded = コマンド { $command } は正常に完了しました。
log-run-failed = コマンド { $command } に失敗しました。
log-run-outcome-unknown = コマンド { $command } は終了しましたが、最終結果は不明です。再試行する前に診断に従って対処してください。
log-run-cancelled = コマンド { $command } をキャンセルしました。
log-performance-counters = パフォーマンスカウンター：SQLite トランザクション制御の試行 { $sqlite_control_attempted_total } 回、候補ツリー全体の検証開始 { $candidate_validation_started } 回、完了 { $candidate_validation_completed } 回。
log-lua-print = Lua：{ $message }
log-plan-resolved = コマンド { $command } のプラン元: { $source }。
log-phase-started = フェーズ開始: { $phase }。
log-retry-summary = { $count } 回再試行しました。
log-translation-task-started = 翻訳タスク { $index }/{ $total } を開始しました。
log-translation-task-finished = 翻訳タスク { $index } は結果 { $outcome } で終了しました。{ $provider_status ->
    [present] 上流プロバイダー：{ $provider }。
   *[missing] 上流プロバイダー：未提供。
}
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
log-task-outcome-value = { $outcome ->
    [complete] 完了
    [partial] 一部完了
    [unavailable] 利用不可
    [failed] 失敗
    [not_committed_after_earlier_failure] 先行失敗により未コミット
    [cancelled] キャンセル
   *[other] 不明な結果で終了
}
diagnostic-object = 対象：{ $subject }
diagnostic-error-heading = エラー：
diagnostic-warning-heading = 警告：
diagnostic-explanation = 原因：{ $reason }
diagnostic-impact = 影響：{ $impact }
diagnostic-resolution = 対処：{ $action }
diagnostic-related = { $relation ->
    [cleanup] 同時にクリーンアップにも失敗しました：
    [rollback] 同時にロールバックにも失敗しました：
    [discard] 同時に候補の破棄にも失敗しました：
    [finalization] 同時に終了処理にも失敗しました：
    [shutdown] 同時にシャットダウンにも失敗しました：
    [observability] 同時に結果の表示または記録にも失敗しました：
   *[other] 同時に関連処理にも失敗しました：
}
diagnostic-impact-value = { $effect ->
    [unchanged] 業務状態は変更されていません
    [progress_preserved] 以前に確認された進捗は保持されています。示された内容は完了していません
    [applied] 関連する業務結果はすでに反映されています
    [applied_run_plan_not_saved] 業務結果は反映されましたが、今回の実行プランは保存されていません
    [applied_finalization_failed] 業務結果は反映されましたが、必要な終了処理が完了していません
    [recovery_required] 結果は確定していますが、示された復旧現場を先に処理する必要があります
    [outcome_unknown] この操作が反映されたか確認できません。対処に従う前に再試行したり復旧物を削除したりしないでください
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] 指定された設定項目を修正して再試行してください
    [fix_input] 指定された入力を修正して再試行してください
    [fix_placeholder_rules] 指定された Placeholder ルールを修正して再試行してください
    [review_translation] 指摘された翻訳を確認し、修正が必要な場合は Manual を使用してください
    [review_disabled_rules] これが意図した結果なら対応は不要です。そうでなければ、指定されたファイルに有効なルールを追加して Extract を再実行してください
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
    [empty_text_capture] text キャプチャが空です
    [rules_owner_disabled] 選択した Rules ファイルは rule = [] を使用しています。Rules は無効化され、抽出済みアセットは削除されました
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
    [stdout_write_failed] 標準出力に書き込めませんでした
    [stderr_write_failed] 標準エラー出力に書き込めませんでした
    [stdout_flush_failed] 標準出力をフラッシュできませんでした
    [stderr_flush_failed] 標準エラー出力をフラッシュできませんでした
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
    [not_regular_file] 既存の対象は通常のファイルではありません
    [wrong_publisher_instance] 公開トークンは別の公開器インスタンスに属しています
    [journal_corrupt] 公開復旧ジャーナルが無効または不完全です
    [unexpected_artifact] 予期しないファイルシステム成果物が操作を妨げています
    [interactive_session_already_open] 別の対話型 SQLite セッションがすでに実行中です
    [backup_incomplete] SQLite バックアップが完了状態に達しませんでした
    [request_serialization_failed] モデルリクエストをシリアル化できませんでした
    [http_client_build_failed] モデルサービスの HTTP クライアントを作成できませんでした
    [dns_resolution_failed] DNS 名前解決に失敗しました
    [tcp_connection_failed] TCP 接続に失敗しました
    [request_send_failed] HTTP リクエストを送信できませんでした
    [response_read_failed] HTTP 応答を読み取れませんでした
    [tls_handshake_failed] TLS ハンドシェイクに失敗しました
    [connect_timed_out] TCP 接続がタイムアウトしました
    [read_timed_out] HTTP 応答の読み取りがタイムアウトしました
    [request_timed_out] HTTP リクエストが総タイムアウトを超えました
    [response_decode_failed] HTTP 応答をデコードできませんでした
    [redirect_rejected] HTTP リダイレクトが拒否されました
    [response_parsing_failed] モデル応答が有効な JSON ではありません
    [model_stream_invalid_json] モデルストリームのイベントが有効な JSON ではありません
    [model_stream_invalid_utf8] モデルストリームに無効な UTF-8 が含まれています
    [model_stream_error_event] モデルストリームがサービスエラーイベントを返しました
    [model_stream_unclosed_event] SSE イベントが空行で閉じられていません
    [model_stream_missing_finish] Chat ストリームに finish_reason がありません
    [model_stream_missing_responses_terminal] Responses ストリームに終端イベントがありません
    [model_stream_event_type_mismatch] SSE イベント名と JSON type が一致しません
    [model_stream_duplicate_choice] モデルストリームが同じ choice を重複して返しました
    [model_stream_choice_after_finish] Chat ストリームが finish 後に応答を変更するフィールドを送信しました
    [model_stream_unexpected_done] Responses ストリームが予期しない [DONE] を返しました
    [response_json_invalid] Assistant 応答は有効な JSON ではありません
    [response_shape_invalid] Assistant JSON のルートまたは応答構造が要件と一致しません
    [response_id_invalid] 応答項目の output ID が無効です
    [response_id_unexpected] 応答に要求していない output ID が含まれています
    [response_id_duplicate] 応答に同じ output ID が複数あります
    [response_id_missing] 応答に要求した output ID がありません
    [response_translation_not_array] translation は文字列配列である必要があります
    [response_translation_item_not_string] translation 配列に文字列でない項目があります
    [response_echo_shape_invalid] エコーされた source オブジェクトが要求された source/translation 構造と一致しません
    [response_echo_source_item_not_string] エコーされた source 配列に文字列でない項目があります
    [response_translation_blank] 返された翻訳が空です
    [response_translation_text_invalid] 返された翻訳に許可されない改行、NUL、またはバイトオーダーマークがあります
    [response_placeholder_snapshot_invalid] 応答検証に使用した Placeholder スナップショットが無効です
    [response_placeholder_identity_or_count_mismatch] 翻訳が必須 Placeholder の識別情報または個数を変更しました
    [response_placeholder_missing] 翻訳に必須の制御 token がありません
    [response_placeholder_unexpected] 翻訳に予定外の制御 token があります
    [response_placeholder_order_mismatch] 翻訳が必須の制御 token 順序を変更しました
    [response_placeholder_binding_mismatch] 翻訳が必須 Placeholder と本文の対応関係を変更しました
    [response_placeholder_boundary_mismatch] 翻訳が必須 Placeholder の境界を追加または削除しました
    [response_placeholder_reserved_token] 翻訳に予約済み Placeholder token があります
    [response_placeholder_ambiguous] 返された Placeholder を必須 token に一意に対応付けられません
    [response_control_token_invalid] 返された制御 token の構造が無効です
    [response_text_segment_count_mismatch] 応答が必須テキストセグメント数を変更しました
    [response_text_segment_shape_mismatch] 応答が必須テキストセグメント構造を変更しました
    [response_line_count_mismatch] translation 配列の項目数が要件と一致しません
    [response_line_text_invalid] translation 配列の項目に受理できないテキストがあります
    [response_blank_line_mismatch] translation 配列が必須の空スロットと非空スロットの位置を保持していません
    [response_source_residual] 受理された翻訳に原文言語が残っているため確認が必要です
    [response_finish_requires_review] モデルが最終状態以外の理由で停止したため、返された結果の確認が必要です
    [response_thinking_empty] 必須の think フィールドが空か、空白文字のみです
    [response_no_usable_output] Assistant 応答に使用可能な出力がありません
    [response_all_outputs_rejected] Assistant 応答のすべての出力が拒否されました
    [invalid_response_contract] モデル応答が必要な応答契約を満たしていません
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
diagnostic-http-route-direct = 直接接続（プロキシなし）
diagnostic-http-route-proxy = 明示的なプロキシ { $proxy } 経由
diagnostic-retry-after = Retry-After：{ $seconds } 秒
diagnostic-provider-code = プロバイダー code：{ $code }
diagnostic-provider-type = プロバイダー type：{ $kind }
diagnostic-provider-message = プロバイダーのメッセージ：{ $message }
diagnostic-json-position = { $line } 行、{ $column } 列
diagnostic-response-item = 応答項目 { $item }
diagnostic-array-item = 配列項目 { $item }
diagnostic-token-position = 制御 token の位置 { $position }
diagnostic-text-segment = テキストセグメント { $segment }
diagnostic-post-finish-fields = finish 後のフィールド：{ $fields }
diagnostic-expected-actual = 期待値 { $expected }、実際値 { $actual }
diagnostic-placeholder-rule-file = { $path } の Placeholder ルール { $number }
diagnostic-placeholder-rule-project = 現在のプロジェクトの Placeholder ルール { $number }
manual-exported = { $entries } 件を { $path } にエクスポートしました
manual-checked = 有効 { $valid }、未入力 { $unfilled }、エラー { $errors }
manual-applied = 適用 { $applied }、未入力 { $unfilled }、エラー { $errors }
manual-value = { $code ->
    [invalid_source_line] source の { $line } 番目に改行または NUL が含まれています
    [invalid_translation_line] translation の { $line } 番目に改行または NUL が含まれています
    [fixed_length] fixed 訳には { $expected } 項必要ですが、{ $actual } 項あります
    [fixed_blank_slot] fixed 訳の { $line } 番目は空のままにしてください
    [rerun_export] manual export を再実行してください
    [rerun_export_without_controls] manual export を再実行し、配列項目に改行や NUL を入れないでください
    [rerun_export_then_fill] manual export を再実行してから訳文を入力してください
    [resolve_temporary_then_rerun_export] 表示された固定一時パスを確認し、残っているオブジェクトがあれば削除してから manual export を再実行してください
    [resolve_published_backup_cleanup] 2 つのエクスポートは適用済みです。出力を確認してから、表示された固定 backup ファイルを削除してください
    [keep_exported_type] manual export が出力した type を保持してください
   *[other] __ATT_FALLBACK__
}
task-record-title = 翻訳タスク
task-record-final-result-heading = 最終結果
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
task-record-provider = 上流プロバイダー：{ $provider }
task-record-provider-unavailable = 上流プロバイダー：未提供
task-record-requested = 要求された翻訳：{ $requested } 項目
task-record-accepted-written = 受理：{ $accepted } 項目（ID：{ $ids }）、実位置 { $written } 箇所へ書き込み
task-record-accepted-outcome-unknown = 検収済み：{ $accepted } 項目（ID：{ $ids }）；データベースのコミット結果を確認できません
task-record-unaccepted = 未受理：{ $unaccepted } 項目（ID：{ $ids }）
task-record-task-diagnostic = タスク診断
