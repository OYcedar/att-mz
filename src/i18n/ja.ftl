app-about = 再利用可能なプロジェクト状態で RPG Maker ゲームを翻訳します
cli-config-help = 今回の実行で使用する厳密な TOML 設定ファイル
cli-ui-language-help = ヘルプ、診断、進捗、結果、プロジェクトログの言語: ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko、vi
cli-progress-help = 進捗表示モード: auto、plain、off
cli-mz-about = RPG Maker MZ ゲームを翻訳します
cli-mv-about = RPG Maker MV ゲームを翻訳します
cli-init-about = 名前付きゲームプロジェクトを初期化または更新します
cli-extract-about = 明示または保存済み owner プランで原文を抽出します
cli-translate-about = 明示または保存済み Profile で抽出済み原文を翻訳します
cli-write-back-about = 承認済み訳文をゲームへ書き戻します
cli-project-name-help = 安定したプロジェクト名
cli-init-path-help = RPG Maker ゲームのルート。既存プロジェクトでは前回成功時のパスを再利用できます
cli-source-language-help = 原文の言語 ID
cli-target-language-help = 翻訳先の言語 ID
cli-dialogue-width-help = 会話行あたりの最大全角文字数
cli-scrolling-width-help = スクロールテキスト行あたりの最大全角文字数
cli-help-width-help = ヘルプまたは説明行あたりの最大全角文字数
cli-builtin-help = ATT 内蔵の RPG Maker テキスト位置を使用します
cli-rules-help = Rules owner をこの TOML 定義で置換します。空のルール一覧で無効になります
cli-dialogue-rules-help = Builtin と併用する MV 会話名投影を置換します
cli-lua-help = このフェーズの Lua プログラムを置換します。0 バイトのファイルで消去します
cli-profile-help = 翻訳 Profile ID。省略すると前回成功した Profile を再利用します
cli-terms-help = プロジェクトの用語リソースを置換します
cli-placeholders-help = プロジェクトの Placeholder リソースを置換します
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
log-label-phase-plan-standard = 標準書き戻し計画
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
notice-translate-reuse-lua = Lua オプションが指定されなかったため、前回成功した Translate Lua の選択を再利用します。
notice-write-back-reuse-lua = Lua オプションが指定されなかったため、前回成功した WriteBack Lua プログラムを再利用します。
notice-write-back-standard-only = WriteBack Lua プログラムは未設定です。Standard のみ実行します。
notice-owner-disabled = owner { $owner } を無効にし、今後の自動プランから削除しました。
notice-lua-cleared = { $phase } Lua プログラムを消去しました。今回は実行しません。
notice-no-model-request = すべての標準翻訳単位が最新のため、今回 Standard はモデルへのリクエストを行いませんでした。
notice-manual-layout = { $count } 単位で改行の手動確認が必要です。
notice-log-degraded = プロジェクトログを利用できないか劣化しています。コマンドは継続し、終了状態には影響しません。
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
progress-extract-lua = Extract Lua プログラムを実行しています
progress-extract-commit = 抽出資産をコミットしています
progress-translate-planning = 翻訳タスクを計画しています
progress-translate-confirmed = 確認済みの翻訳タスク
progress-translate-no-work = モデル呼び出しは不要です
progress-write-back-read-assets = 承認済み資産を読み込んでいます
progress-write-back-planning = 文書書き換えを計画しています
progress-write-back-documents = 文書を書き換えました
progress-write-back-lua = WriteBack Lua プログラムを実行しています
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
result-translate-standard = 標準翻訳: タスク { $total }、完全 { $complete }、部分 { $partial }、利用不可 { $unavailable }。{ $written } 箇所を書き込み、残り { $remaining } 箇所
result-translate-convergence = 状態収束: 保持 { $retained }、無効化 { $invalidated }、非該当 { $not_applicable }、再利用 { $reused }
result-write-back-completed = 書き戻し完了: { $project }
result-output-directory = 出力ディレクトリ: { $path }
result-write-back-standard = 標準書き戻し: 訳文 { $translated } 単位、原文 { $original } 単位。自動折返し { $auto_wrapped }、改行追加 { $breaks }、全角インデント追加 { $indents }。手動配置 { $manual }
result-lua-executed = Lua: 実行済み
result-lua-not-executed = Lua: 未実行
result-cancelled = 安全な終了処理後にコマンドをキャンセルしました。
result-plan-saved = 成功した実行プランを保存しました。
result-translate-plan-sources = 今回成功した実行プランを保存しました。Profile の指定元: { $profile_source }、Lua の指定元: { $lua_source }。
log-run-started = コマンド { $command } を開始しました。
log-run-succeeded = コマンド { $command } は正常に完了しました。
log-run-failed = コマンド { $command } に失敗しました。
log-run-outcome-unknown = コマンド { $command } は終了しましたが、最終結果は不明です。エラーに示された復旧場所を確認してください。
log-run-cancelled = コマンド { $command } をキャンセルしました。
log-performance-counters = パフォーマンスカウンター：SQLite トランザクション制御の試行 { $sqlite_control_attempted_total } 回、候補ツリー全体の検証開始 { $candidate_validation_started } 回、完了 { $candidate_validation_completed } 回。
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
    [process_output] プロセス出力
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
