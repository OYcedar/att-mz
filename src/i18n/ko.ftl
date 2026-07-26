app-about = 재사용 가능한 프로젝트 상태로 RPG Maker 게임을 번역합니다
cli-config-help = 이번 실행에 사용할 엄격한 TOML 구성 파일
cli-ui-language-help = 도움말, 진단, 진행률, 결과 및 프로젝트 로그의 언어: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko 또는 vi
cli-progress-help = 실시간 진행률 모드: auto, plain 또는 off
cli-mz-about = RPG Maker MZ 게임 번역
cli-mv-about = RPG Maker MV 게임 번역
cli-init-about = 이름이 지정된 게임 프로젝트 초기화 또는 업데이트
cli-extract-about = 명시적 또는 저장된 owner 계획으로 원문 추출
cli-translate-about = 명시적 또는 저장된 Profile로 추출된 원문 번역
cli-write-back-about = 승인된 번역을 게임에 다시 쓰기
cli-project-lua-about = 프로젝트 컨텍스트에서 신뢰할 수 있는 Lua 프로그램을 한 번 실행
cli-project-name-help = 안정적인 프로젝트 이름
cli-init-path-help = RPG Maker 게임 루트. 기존 프로젝트는 마지막 성공 경로를 재사용할 수 있습니다
cli-source-language-help = 원문 언어 ID
cli-target-language-help = 대상 언어 ID
cli-dialogue-width-help = 대화 줄당 최대 전각 문자 수
cli-scrolling-width-help = 스크롤 텍스트 줄당 최대 전각 문자 수
cli-help-width-help = 도움말 또는 설명 줄당 최대 전각 문자 수
cli-builtin-help = ATT 내장 RPG Maker 텍스트 위치 사용
cli-rules-help = 이 TOML 정의로 Rules owner 교체. 빈 규칙 목록은 비활성화합니다
cli-dialogue-rules-help = Builtin과 함께 쓰는 MV 대화 이름 투영 교체
cli-lua-help = 현재 단계 Lua 프로그램 교체. 0바이트 파일은 프로그램을 지웁니다
cli-profile-help = 번역 Profile ID. 생략하면 마지막 성공 Profile을 재사용합니다
cli-terms-help = 프로젝트 용어 리소스 교체
cli-placeholders-help = 프로젝트 Placeholder 리소스 교체
cli-project-lua-profile-help = Standard 수동 승인용 Profile. 생략하면 Standard를 열 때 마지막으로 성공한 Translate Profile을 재사용합니다
cli-project-lua-script-help = 한 번 실행할 신뢰할 수 있는 Lua 프로그램
cli-project-lua-arguments-help = -- 뒤에서 Lua arg[1..]에 전달할 UTF-8 인수
cli-usage-heading = 사용법:
cli-commands-heading = 명령:
cli-options-heading = 옵션:
cli-arguments-heading = 인수:
cli-options-metavar = 옵션
cli-command-metavar = 명령
cli-print-help = 도움말 출력
cli-print-version = 버전 출력
cli-missing-config = 필수 구성 경로 --config <FILE>이 없습니다.
cli-blank-value = 값은 비워 둘 수 없습니다.
cli-invalid-positive-integer = 값은 양의 정수여야 합니다.
cli-invalid-progress = 지원하지 않는 진행률 모드 { $value }입니다. auto, plain 또는 off를 사용하세요.
cli-invalid-ui-language-argument = --ui-language에 잘못된 언어 태그가 있습니다: { $value }.
cli-unsupported-ui-language-argument = --ui-language가 지원하지 않는 언어를 요청했습니다: { $value }.
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE에 잘못된 언어 태그가 있습니다: { $value }.
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE가 지원하지 않는 언어를 요청했습니다: { $value }.
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE가 올바른 Unicode가 아닙니다.
cli-unexpected-argument = 예상하지 못한 인수: { $value }.
cli-missing-required-argument = 필수 인수가 없습니다: { $value }.
cli-invalid-value = { $argument }의 값 { $value }이 잘못되었습니다.
cli-error-heading = 오류:
cli-try-help = 자세한 내용은 --help를 사용하세요.
cli-missing-value = { $argument }에 값을 제공해야 합니다.
cli-missing-subcommand = 명령을 제공해야 합니다.
cli-argument-conflict = { $argument }은(는) 함께 제공된 다른 인수와 사용할 수 없습니다.
cli-wrong-number-of-values = { $argument }에 제공된 값의 개수가 올바르지 않습니다.
cli-invalid-utf8 = 명령줄 인수가 올바른 Unicode가 아닙니다.
cli-parse-failure = 명령줄을 해석할 수 없습니다.
log-label-phase-check-project = 프로젝트 확인
log-label-phase-scan-source = 원본 검색
log-label-phase-prepare-candidate = 후보 준비
log-label-phase-update-database = 데이터베이스 업데이트
log-label-phase-publish = 게시
log-label-phase-builtin = 기본 제공 추출
log-label-phase-rules = 규칙 추출
log-label-phase-lua = Lua 처리
log-label-phase-planning = 계획
log-label-phase-confirmed-tasks = 작업 확인
log-label-phase-no-work = 작업 불필요
log-label-phase-read-assets = 자산 읽기
log-label-phase-plan-standard = 표준 쓰기 계획
log-label-phase-rewrite-documents = 문서 다시 쓰기
log-label-phase-validate-candidate = 후보 검증
log-label-task-complete = 완료
log-label-task-partial = 일부 사용 가능
log-label-task-unavailable = 사용 불가
log-label-task-failed = 실패
error-state-applied-finalization = 결과는 적용되었지만 마무리에 실패했습니다. 재시도 전에 프로젝트 상태를 확인하세요.
error-no-executable-extract-owner = 지운 뒤 실행 가능한 Extract owner가 없어 계획을 저장하지 않았습니다.
error-plan-save-failed-applied = 명령 결과는 적용되었지만 새 실행 계획을 저장하지 못했습니다. 다음 실행에서는 의도한 옵션을 명시하세요.
error-plan-save-outcome-unknown = 명령 결과는 적용되었지만 실행 계획 커밋 결과를 확인할 수 없습니다. 다음 실행에서는 의도한 옵션을 명시하세요.
plan-source-explicit = 명시적 입력
plan-source-project-state = 프로젝트 상태
plan-source-product-default = 제품 동작
notice-init-reuse-path = 원본 경로가 없어 마지막 성공 경로를 재사용합니다: { $path }.
notice-extract-reuse-owners = 추출 범위가 없어 마지막 성공 계획을 재사용합니다: { $owners }.
notice-translate-reuse-profile = Profile이 없어 마지막 성공 Profile을 재사용합니다: { $profile }.
notice-translate-reuse-lua = Lua 옵션이 없어 마지막 성공 Translate Lua 선택을 재사용합니다.
notice-write-back-reuse-lua = Lua 옵션이 없어 마지막 성공 WriteBack Lua 프로그램을 재사용합니다.
notice-write-back-standard-only = WriteBack Lua 프로그램이 구성되지 않아 Standard만 실행합니다.
notice-owner-disabled = owner { $owner }을 비활성화하고 이후 자동 계획에서 제거했습니다.
notice-lua-cleared = { $phase } Lua 프로그램을 지웠으며 이번에는 실행하지 않습니다.
notice-no-model-request = 모든 표준 번역 단위가 최신 상태여서 이번 실행에서 Standard는 모델 요청을 보내지 않았습니다.
notice-manual-layout = { $count }개 단위의 줄바꿈을 수동으로 확인해야 합니다.
notice-log-degraded = 프로젝트 로그를 사용할 수 없거나 성능이 저하되었습니다. 명령은 계속되며 종료 상태는 바뀌지 않습니다.
notice-task-records-degraded = 번역 작업 기록을 사용할 수 없거나 성능이 저하되었습니다. 명령은 계속되며 종료 상태는 바뀌지 않습니다.
progress-init-check-project = 프로젝트 상태 확인 중
progress-init-scan-source = 게임 원본 검색 중
progress-init-build-candidate = 프로젝트 후보 구성 중
progress-init-converge-database = 프로젝트 데이터베이스 수렴 중
progress-init-publish = 초기화된 프로젝트 게시 중
progress-save-run-plan = 성공한 실행 계획 저장 중
progress-extract-owner = 추출 owner: { $owner }
progress-extract-documents = 문서 검색 중
progress-extract-builtin = Builtin 작업 단위
progress-extract-rules = Rules 정의
progress-extract-lua = Extract Lua 프로그램 실행 중
progress-extract-commit = 추출 자산 커밋 중
progress-translate-planning = 번역 작업 계획 중
progress-translate-confirmed = 확인된 번역 작업
progress-translate-no-work = 모델 요청이 필요하지 않음
progress-project-lua = 프로젝트 Lua 프로그램 실행 중
progress-write-back-read-assets = 승인된 자산 읽는 중
progress-write-back-planning = 문서 다시 쓰기 계획 중
progress-write-back-documents = 문서 다시 쓰기
progress-write-back-lua = WriteBack Lua 프로그램 실행 중
progress-write-back-validate-candidate = 출력 후보 검증 중
progress-write-back-publish = 출력 게시 중. 중단 후에도 확인된 결과를 기다립니다
progress-finalizing = 필수 마무리 작업 중
progress-safe-stopping = 안전하게 중지 중. 마지막으로 확인된 진행률을 유지합니다
result-init-completed = 초기화 완료: { $project }
result-init-created = 프로젝트 상태: 생성됨
result-init-unchanged = 프로젝트 상태: 변경 없음
result-init-updated = 프로젝트 상태: 업데이트됨
result-init-stale-owners = 다시 추출 필요: { $owners }
result-extract-completed = 추출 완료: { $project }
result-translate-completed = 번역 완료: { $project }(Profile: { $profile })
result-translate-standard = 표준 번역: 작업 { $total }, 완료 { $complete }, 부분 { $partial }, 사용 불가 { $unavailable }; { $written }개 위치 기록, { $remaining }개 남음
result-translate-convergence = 상태 수렴: 유지 { $retained }, 무효화 { $invalidated }, 해당 없음 { $not_applicable }, 재사용 { $reused }
result-write-back-completed = 쓰기 완료: { $project }
result-project-lua-completed = 프로젝트 Lua 실행 완료: { $project }
result-output-directory = 출력 디렉터리: { $path }
result-write-back-standard = 표준 쓰기: 번역 { $translated }단위, 원문 { $original }단위; 자동 줄바꿈 { $auto_wrapped }, 줄바꿈 추가 { $breaks }, 전각 들여쓰기 추가 { $indents }; 수동 배치 { $manual }
result-lua-executed = Lua: 실행됨
result-lua-not-executed = Lua: 실행 안 함
result-cancelled = 안전한 마무리 후 명령을 취소했습니다.
result-plan-saved = 성공한 실행 계획을 저장했습니다.
result-translate-plan-sources = 이번에 성공한 실행 계획을 저장했습니다. Profile 출처: { $profile_source }; Lua 출처: { $lua_source }.
log-run-started = 명령 { $command }이 시작되었습니다.
log-run-succeeded = 명령 { $command }이 성공적으로 완료되었습니다.
log-run-failed = 명령 { $command }이 실패했습니다.
log-run-outcome-unknown = 명령 { $command }이 종료되었지만 최종 결과를 알 수 없습니다. 오류에 표시된 복구 위치를 따르십시오.
log-run-cancelled = 명령 { $command }이 취소되었습니다.
log-performance-counters = 성능 카운터: SQLite 트랜잭션 제어 시도 { $sqlite_control_attempted_total }회, 전체 후보 트리 검증 시작 { $candidate_validation_started }회, 완료 { $candidate_validation_completed }회.
log-plan-resolved = 명령 { $command }의 계획 출처: { $source }.
log-phase-started = 단계 시작: { $phase }.
log-phase-finished = 단계 완료: { $phase }.
log-retry-summary = { $count }회 재시도했습니다.
log-no-work = 작업이 필요하지 않았습니다: { $reason }.
log-no-work-translation-up-to-date = 번역이 현재 원본 및 프로필과 일치합니다
log-partial-result = 주의가 필요한 부분 결과가 { $count }개 있습니다.
log-translation-task-started = 번역 작업 { $index }/{ $total } 시작.
log-translation-task-finished = 번역 작업 { $index }이 결과 { $outcome }으로 종료되었습니다.
log-translation-task-diagnostic = 번역 작업 { $index }이 { $attempts }회 시도 후 진단을 보고했습니다: { $diagnostic }
diagnostic-title = 오류 [{ $code }]
diagnostic-stage = 단계: { $stage }
diagnostic-subject = 위치: { $subject }
diagnostic-subject-value = { $kind ->
    [command] 명령 { $value }
    [field] 필드 { $value }
    [project] 프로젝트 { $value }
    [profile] 프로필 { $value }
    [component] 구성 요소 { $value }
   *[other] { $value }
}
diagnostic-reason = 원인: { $reason }
diagnostic-impact = 영향: { $impact }
diagnostic-action = 조치: { $action }
diagnostic-recovery = 복구 위치: { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] 구성 요소 { $value }
    [transaction] 트랜잭션 { $value }
   *[other] { $value }
}
diagnostic-related = 관련 오류 { $index }:
diagnostic-stage-value = { $code ->
    [process_startup] 프로세스 시작
    [process_output] 프로세스 출력
    [configuration] 구성 불러오기
    [command_preparation] 명령 준비
    [project_opening] 프로젝트 열기
    [init] 초기화
    [extract] 추출
    [translate] 번역
    [write_back] 쓰기 반영
    [lua] 프로젝트 Lua 실행
    [model_request] 모델 요청
    [run_plan_finalization] 실행 계획 마무리
    [publication] 게시
    [shutdown] 종료
    [logging] 프로젝트 로그
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] 상태가 변경되지 않았습니다
    [valid_progress_preserved] 유효한 진행 상황을 보존했습니다
    [result_applied_but_run_plan_not_saved] 결과는 적용되었지만 실행 계획은 저장되지 않았습니다
    [state_applied_but_finalization_failed] 상태는 적용되었지만 마무리가 완료되지 않았습니다
    [recovery_required] 상태를 신뢰하기 전에 복구가 필요합니다
    [outcome_unknown] 최종 상태를 알 수 없습니다
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] 표시된 구성 필드를 수정한 후 다시 시도하세요
    [fix_input] 표시된 입력을 수정한 후 다시 시도하세요
    [check_path_and_permissions] 경로, 파일 시스템 상태 및 권한을 확인하세요
    [check_project_state] 프로젝트 상태를 확인하고 수정한 후 다시 시도하세요
    [retry_after_resolving_contention] 충돌하는 작업이 끝날 때까지 기다린 후 다시 시도하세요
    [check_model_service] 모델 서비스 응답과 계정 한도를 확인하세요
    [preserve_recovery_artifacts] 나열된 복구 산출물을 삭제하지 말고, 출력을 복구한 후 다시 시도하세요
    [retry] 작업을 다시 시도하세요
    [report_bug] 오류 코드와 로그 경로를 포함하여 ATT 결함을 보고하세요
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 필수 값이 없습니다
    [extract_plan_required] 재사용할 수 있는 Extract 계획이 저장되어 있지 않습니다. --builtin, --rules 또는 --lua 중 하나 이상을 지정하세요
    [conflicting_values] 제공한 값이 서로 충돌합니다
    [invalid_syntax] 값의 구문이 잘못되었습니다
    [invalid_encoding] 텍스트 인코딩이 잘못되었습니다
    [invalid_value] 값이 필수 계약을 위반합니다
    [not_found] 필요한 객체가 없습니다
    [busy] 다른 작업이 리소스를 사용 중입니다
    [state_mismatch] 저장된 프로젝트 상태가 이 작업의 요구 사항을 충족하지 않습니다
    [requirement_failed] 필수 선행 조건이 충족되지 않았습니다
    [transaction_rolled_back] 트랜잭션이 실패하여 변경 사항을 롤백했습니다
    [transaction_outcome_unknown] 트랜잭션의 커밋 또는 롤백 결과를 확인할 수 없습니다
    [finalization_failed] 작업 결과는 존재하지만 마무리에 실패했습니다
    [rollback_failed] 기본 작업과 롤백이 모두 실패했습니다
    [external_service_rejected] 외부 서비스가 요청을 거부했습니다
    [external_service_unavailable] 외부 서비스를 사용할 수 없습니다
    [executor_closed] 실행 서비스가 종료 중이거나 이미 종료되었습니다
    [concurrent_shutdown] 다른 호출자가 실행기를 종료하고 있습니다
    [executor_state_poisoned] 실행기 수명 주기 상태가 손상되었습니다
    [worker_spawn_failed] 운영 체제가 작업자 스레드를 만들 수 없습니다
    [worker_channel_closed] 마무리가 끝나기 전에 작업자 명령 채널이 닫혔습니다
    [worker_panicked] 작업자가 예기치 않게 종료되었습니다
    [reparse_point_forbidden] 경로에 신뢰할 수 없는 재분석 지점이 있습니다
    [non_local_volume] 경로가 로컬 고정 볼륨에 있지 않습니다
    [non_ntfs_volume] 경로가 NTFS 볼륨에 있지 않습니다
    [case_sensitive_directory] 디렉터리에 대소문자를 구분하는 이름 의미 체계가 적용되어 있습니다
    [lock_cancelled] 필수 잠금 대기가 취소되었습니다
    [target_already_exists] 대상이 이미 존재합니다
    [file_identity_changed] 작업 중 파일 ID가 변경되었습니다
    [invalid_path] 경로가 이 작업에 유효한 대상이 아닙니다
    [wrong_publisher_instance] 게시 토큰이 다른 게시자 인스턴스에 속합니다
    [journal_corrupt] 게시 복구 저널이 잘못되었거나 불완전합니다
    [unexpected_artifact] 예기치 않은 파일 시스템 산출물이 작업을 막고 있습니다
    [interactive_session_already_open] 다른 대화형 SQLite 세션이 이미 활성 상태입니다
    [backup_incomplete] SQLite 백업이 완료 상태에 도달하지 못했습니다
    [request_serialization_failed] 모델 요청을 직렬화할 수 없습니다
    [response_parsing_failed] 모델 응답이 유효한 JSON이 아닙니다
    [invalid_response_contract] 모델 응답이 필수 응답 계약을 충족하지 않습니다
    [transport_failed] 유효한 응답을 받기 전에 HTTP 전송이 실패했습니다
    [lua_database_open_failed] Lua 호스트가 프로젝트 데이터베이스 세션을 열 수 없습니다
    [lua_context_creation_failed] Lua 런타임이 VM 컨텍스트를 만들 수 없습니다
    [lua_compilation_failed] Lua 주 프로그램을 컴파일할 수 없습니다
    [lua_execution_failed] Lua 주 프로그램 실행 중 오류가 발생했습니다
    [lua_host_call_failed] Lua 호스트 기능 호출에 실패했습니다
    [lua_finalization_failed] Lua 호스트가 바인딩된 모든 리소스를 마무리할 수 없습니다
    [lua_unclosed_transaction] Lua 프로그램 종료 시 트랜잭션이 열려 있어 롤백했습니다
    [lua_snapshot_store_failed] 검증된 Lua 추출 스냅샷을 커밋할 수 없습니다
    [rules_definition_invalid] Rules 프로그램이 Rules 정의 계약을 충족하지 않습니다
    [rules_document_read_failed] Rules 프로그램에 필요한 원본 문서를 읽을 수 없습니다
    [rules_no_non_blank_match] Rules 항목이 공백이 아닌 의미 단위를 만들지 못했습니다
    [rules_invalid_target] Rules 항목이 텍스트 대상으로 사용할 수 없는 값을 선택했습니다
    [rules_pattern_match_failed] Rules PCRE2 패턴을 평가할 수 없습니다
    [rules_zero_width_match] Rules 패턴이 너비가 0인 일치를 만들었습니다
    [rules_overlapping_capture] Rules 패턴이 겹치는 텍스트 캡처를 만들었습니다
    [rules_missing_text_capture] 필수 명명 텍스트 캡처가 일치에 참여하지 않았습니다
    [rules_invalid_capture_range] Rules 일치 또는 캡처 범위가 유효한 UTF-8 문자 경계를 벗어났습니다
    [rules_duplicate_target] 두 Rules 항목이 같은 실제 텍스트 대상을 요구합니다
    [rules_invalid_materialization] Rules 투영 레시피로 원본 값을 재구성할 수 없습니다
    [rules_snapshot_invalid] 추출된 Rules 그룹이 유효한 자산 스냅샷을 구성하지 않습니다
    [rules_snapshot_store_failed] 검증된 Rules 추출 스냅샷을 커밋할 수 없습니다
    [write_back_extraction_out_of_date] 추출한 자산이 현재 프로젝트 원본과 더 이상 일치하지 않습니다
    [write_back_asset_snapshot_invalid] 저장된 Standard 자산이 유효한 쓰기 반영 스냅샷을 구성하지 않습니다
    [source_document_invalid] RPG Maker 원본 문서가 필수 문서 형식을 충족하지 않습니다
    [write_back_mutation_invalid] 검증된 번역 변경을 고정된 원본 위치에 적용할 수 없습니다
    [write_back_output_path_invalid] 다시 쓴 파일이 허용된 RPG Maker 출력 트리 밖에 있습니다
    [write_back_output_path_duplicate] 둘 이상의 다시 쓴 파일이 같은 출력 경로를 대상으로 합니다
    [write_back_candidate_project_mismatch] 준비된 쓰기 반영 후보가 다른 프로젝트에 속합니다
    [write_back_candidate_invalid] 쓰기 반영 후보가 필수 data/js 트리 구조를 충족하지 않습니다
    [write_back_unexpected_lua_outcome] Lua 쓰기 반영 프로그램이 다른 Lua 단계의 결과를 반환했습니다
    [write_back_not_published] 쓰기 반영 후보가 현재 출력 디렉터리를 대체하지 않았습니다
    [write_back_published_with_residuals] 출력을 게시했지만 일부 복구 산출물을 제거할 수 없습니다
    [write_back_recovery_required] 내용을 신뢰하기 전에 출력 디렉터리를 복구해야 합니다
    [internal_invariant] 내부 불변 조건을 위반했습니다. ATT 결함입니다
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] 찾을 수 없음
    [permission_denied] 권한이 거부됨
    [connection_refused] 연결이 거부됨
    [connection_reset] 연결이 재설정됨
    [host_unreachable] 호스트에 연결할 수 없음
    [network_unreachable] 네트워크에 연결할 수 없음
    [connection_aborted] 연결이 중단됨
    [not_connected] 연결되지 않음
    [address_in_use] 주소가 이미 사용 중임
    [address_not_available] 주소를 사용할 수 없음
    [network_down] 네트워크가 중단됨
    [broken_pipe] 파이프가 끊어짐
    [already_exists] 이미 존재함
    [would_block] 작업이 차단됨
    [not_a_directory] 디렉터리가 아님
    [is_a_directory] 디렉터리임
    [directory_not_empty] 디렉터리가 비어 있지 않음
    [read_only_filesystem] 읽기 전용 파일 시스템
    [stale_network_file_handle] 네트워크 파일 핸들이 만료됨
    [invalid_input] 작업 입력이 잘못됨
    [invalid_data] 데이터가 잘못됨
    [timed_out] 작업 시간 초과
    [write_zero] 쓰기가 진행되지 않음
    [storage_full] 저장 공간이 가득 참
    [not_seekable] 객체에서 탐색할 수 없음
    [quota_exceeded] 저장 공간 할당량 초과
    [file_too_large] 파일이 기반 시스템에서 처리할 수 있는 크기를 초과함
    [resource_busy] 리소스가 사용 중임
    [executable_file_busy] 실행 파일이 사용 중임
    [deadlock] 작업이 교착 상태를 일으킴
    [crosses_devices] 작업이 파일 시스템 장치를 가로지름
    [too_many_links] 파일 시스템 링크가 너무 많음
    [invalid_filename] 파일 이름이 잘못됨
    [argument_list_too_long] 운영 체제 인수 목록이 너무 김
    [interrupted] 작업이 중단됨
    [unsupported] 지원되지 않는 작업
    [unexpected_eof] 예기치 않은 파일 끝
    [out_of_memory] 운영 체제가 메모리를 할당할 수 없음
    [other] 기타 운영 체제 오류
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [runtime_configuration_invalid] 런타임 구성이 잘못되었습니다
    [unsupported_prompt_locale] 소문자 auto 또는 지원되는 BCP 47 UI 로캘이어야 합니다
    [language_policy_term_blank] 언어 정책 용어는 비워 둘 수 없습니다
    [language_policy_term_surrounding_whitespace] 언어 정책 용어 앞뒤에 공백을 둘 수 없습니다
    [language_policy_term_duplicate] 언어 정책 용어는 중복될 수 없습니다
    [quote_repair_candidates_empty] 따옴표 복구 후보 목록은 비워 둘 수 없습니다
    [quote_repair_delimiter_invalid] 따옴표 복구 구분자는 영숫자, 공백 또는 제어 문자일 수 없습니다
    [quote_repair_pair_duplicate] 따옴표 복구 쌍은 중복될 수 없습니다
    [quote_repair_delimiter_ambiguous] 따옴표 복구 구분자는 정확히 하나의 쌍에 속해야 합니다
    [language_id_blank] 언어 ID는 비워 둘 수 없습니다
    [language_id_surrounding_whitespace] 언어 ID 앞뒤에 공백을 둘 수 없습니다
    [language_id_uses_underscore] 언어 ID의 하위 태그 사이에는 하이픈을 사용해야 합니다
    [language_id_invalid_syntax] 언어 ID는 RFC 5646 구문을 충족해야 합니다
    [language_id_invalid_registry_tag] 언어 ID에 잘못된 레지스트리 하위 태그가 있습니다
    [language_id_canonicalization_failed] 언어 ID를 정규화할 수 없습니다
    [language_id_undefined_primary_language] 언어 ID에 기본 언어가 정의되어야 합니다
    [language_id_duplicate] 언어 ID는 고유해야 합니다
    [language_catalog_empty] 원본 언어 모듈이 하나 이상 필요합니다
    [url_invalid] 값은 유효한 URL이어야 합니다
    [url_credentials_forbidden] URL에 자격 증명을 포함할 수 없습니다
    [url_fragment_forbidden] URL에 프래그먼트를 포함할 수 없습니다
    [url_scheme_unsupported] URL 스킴은 http 또는 https여야 합니다
    [api_key_blank] API key는 비워 둘 수 없습니다
    [api_key_surrounding_whitespace] API key 앞뒤에 공백을 둘 수 없습니다
    [api_key_invalid_header] API key를 HTTP Header 값으로 표현할 수 없습니다
    [strict_json_invalid] 값은 엄격한 JSON이어야 합니다(줄={ $line }, 열={ $column })
    [json_object_required] 값은 JSON 객체여야 합니다
    [reserved_request_field] 이 필드는 요청 프로토콜이 소유하므로 재정의할 수 없습니다
    [proxy_must_be_false_or_url] proxy는 false 또는 완전한 http/https URL이어야 합니다
    [pem_path_duplicate] PEM 경로는 고유해야 합니다
    [runtime_maximum_exceeded] 값이 런타임 최댓값을 초과합니다(실제={ $actual }, 최댓값={ $maximum })
    [value_surrounding_whitespace] 값 앞뒤에 공백을 둘 수 없습니다
    [value_blank] 값을 비워 둘 수 없습니다
    [path_blank] 경로를 비워 둘 수 없습니다
    [positive_required] 값은 0보다 커야 합니다(실제={ $actual })
    [usize_range_exceeded] 값이 이 플랫폼의 usize 범위를 초과합니다(실제={ $actual })
    [u32_range_exceeded] 값이 u32 범위를 초과합니다(실제={ $actual })
    [duplicate_profile_id] 번역 프로필 ID는 고유해야 합니다
    [selected_profile_invalid] 선택한 번역 프로필의 구조 또는 필드 형식이 잘못되었습니다
    [referenced_client_not_found] 참조된 LLM 클라이언트가 없습니다
   *[other] __ATT_FALLBACK__
}
diagnostic-io-reason = 작업 { $operation }: { $kind }
diagnostic-io-reason-with-os-code = 작업 { $operation }: { $kind }(OS { $os_code })
diagnostic-io-reason-with-system-message = 작업 { $operation }: { $kind }: { $system_message }
diagnostic-io-reason-with-os-code-and-system-message = 작업 { $operation }: { $kind }(OS { $os_code }): { $system_message }
diagnostic-failure-with-detail = { $failure }: { $detail }
diagnostic-invalid-utf8 = 바이트 { $valid_up_to }의 UTF-8이 잘못되었습니다. 잘못된 길이는 { $error_len }바이트입니다
diagnostic-incomplete-utf8 = 바이트 { $valid_up_to } 뒤의 UTF-8 시퀀스가 불완전합니다
diagnostic-toml-failure-value = { $code ->
    [syntax] TOML 구문이 잘못되었습니다
    [missing_field] 필수 구성 필드가 없습니다
    [unknown_field] 구성에 알 수 없는 필드가 있습니다
    [duplicate_field] 구성 필드가 두 번 이상 선언되었습니다
    [type_mismatch] { $expected }이어야 합니다
    [invalid_value] 구성 값이 필드 계약을 위반합니다
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] 문자열
    [integer] 정수
    [boolean] 부울 값
    [string_or_boolean] 문자열 또는 부울 값
    [string_array] 문자열 배열
    [integer_array] 정수 배열
    [string_pair_array] 문자열 쌍 배열
    [table] 테이블
    [table_array] 테이블 배열
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = 잘못된 TOML({ $resource }): { $failure }
diagnostic-invalid-toml-at = { $line }줄 { $column }열의 TOML이 잘못되었습니다({ $resource }): { $failure }
diagnostic-http-no-details = 모델 서비스 요청이 실패했지만 공개 가능한 HTTP 상태 세부 정보가 없습니다
diagnostic-http-status = HTTP 상태 { $status }
diagnostic-http-retry-after = Retry-After { $seconds }초
diagnostic-http-provider-code = 공급자 오류 코드 { $code }
diagnostic-http-provider-type = 공급자 오류 형식 { $kind }
diagnostic-http-fact-separator = ;{ " " }
diagnostic-sqlite = SQLite 기본 오류 코드 { $primary_code }, 확장 오류 코드 { $extended_code }
diagnostic-windows-status = Windows 작업 { $operation }이 실패했습니다. NTSTATUS { $status }
diagnostic-resource = { $resource }: 실제 { $actual }
diagnostic-resource-with-maximum = { $resource }: 실제 { $actual }, 최댓값 { $maximum }
task-record-title = 번역 작업 { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] 완료
    [partial] 부분 완료
    [unavailable] 사용 불가
    [execution_failed] 실행 실패
    [commit_preparation_failed] 커밋 준비 실패
    [commit_not_applied] 커밋 미적용
    [commit_outcome_unknown] 커밋 결과 알 수 없음
    [not_committed_after_earlier_failure] 이전 실패로 미커밋
    [invalid_result] 잘못된 Executor 결과 순서
    [cancelled] 취소됨
   *[other] { $state }
}
task-record-summary-with-written = `작업 { $ordinal }/{ $total }` · `시도 { $attempts }회` · `검수 { $accepted }/{ $expected }` · `{ $written }곳에 기록`
task-record-summary-without-written = `작업 { $ordinal }/{ $total }` · `시도 { $attempts }회` · `검수 { $accepted }/{ $expected }`
task-record-run-id-label = Run ID:
task-record-started-at-label = 시작 시간:
task-record-duration-label = 총 소요 시간:
task-record-endpoint-label = Endpoint:
task-record-model-label = Model:
task-record-custom-parameters-heading = 사용자 지정 매개변수
task-record-attempts-heading = 요청 과정
task-record-final-result-heading = 최종 결과
task-record-no-request = 전송 가능한 모델 요청이 만들어지지 않았습니다.
task-record-empty-assistant = 모델이 빈 객체를 반환했습니다.
task-record-parse-error = 구문 분석 오류: { $kind ->
    [json] 모델 응답 JSON이 올바르지 않습니다(범주 `{ $category }`, { $line }행 { $column }열)
    [thinking_not_allowed] 현재 응답 모드에서는 사고 출력을 허용하지 않습니다({ $line }행 { $column }열)
    [thinking_envelope_missing] 필수 사고 봉투가 없습니다({ $line }행 { $column }열)
    [thinking_envelope_unclosed] 사고 봉투가 닫히지 않았습니다({ $line }행 { $column }열)
    [thinking_empty] 사고 내용이 비어 있습니다({ $line }행 { $column }열)
    [thinking_nested] 중첩된 사고 봉투가 있습니다({ $line }행 { $column }열)
    [thinking_repeated] 사고 봉투가 반복되었습니다({ $line }행 { $column }열)
    [markdown_fence_no_body] Markdown 펜스에 본문이 없습니다({ $line }행 { $column }열)
    [markdown_fence_unsupported] 언어 표시가 없거나 json 표시인 단일 Markdown 펜스만 허용됩니다({ $line }행 { $column }열)
    [markdown_fence_unclosed] Markdown 펜스가 닫히지 않았습니다({ $line }행 { $column }열)
   *[markdown_fence_invalid_closing] Markdown 펜스는 마지막 독립 행에서 닫혀야 합니다({ $line }행 { $column }열)
}
task-record-attempt-succeeded = 시도 { $number }: 성공; finish reason { $finish_reason }
task-record-attempt-token-usage = ; token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; 소요 시간 `{ $duration }`
task-record-attempt-request-id = ; request ID { $request_id }
task-record-attempt-response-id = ; response ID { $response_id }
task-record-attempt-retryable = 시도 { $number }: 재시도 가능한 요청 실패; 진단 `{ $code }`; 소요 시간 `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; `{ $duration }` 후 재시도
task-record-attempt-wait-completed = ; `{ $duration }` 대기는 완료되었지만 다음 시도는 시작되지 않음
task-record-attempt-wait-cancelled = ; `{ $duration }` 대기 중 취소됨
task-record-attempt-failed = 시도 { $number }: 요청 또는 응답 처리 실패; 진단 `{ $code }`; 소요 시간 `{ $duration }`
task-record-attempt-cancelled = 시도 { $number }: 취소됨; 소요 시간 `{ $duration }`
task-record-structured-reason = 원인: { $reason }
task-record-final-status = 상태: { $state ->
    [complete] 완료, 커밋 확인됨
    [partial] 부분 완료, 커밋 확인됨
    [unavailable] 사용 불가, 프로젝트 변경 없음
    [execution_failed] 실행 실패, 미커밋
    [commit_preparation_failed] 커밋 준비 실패, 미적용 확인
    [commit_not_applied] 트랜잭션 미적용 확인
    [commit_outcome_unknown] 커밋 결과 알 수 없음
    [not_committed_after_earlier_failure] 이전 작업 실패로 미커밋
    [invalid_result] 잘못된 Executor 결과 순서, 미커밋
    [cancelled] 취소됨, 미커밋
   *[other] { $state }
}
task-record-accepted-written = 수락: { $accepted }개 항목, 실제 위치 { $written }곳에 기록
task-record-accepted-outcome-unknown = 검수 완료: { $accepted }개 항목; 데이터베이스 커밋 결과를 확인할 수 없음
task-record-rejected-heading = 수락되지 않음:
task-record-rejected-item = { $id }: { $reason }
task-record-protocol-diagnostic = 프로토콜 진단: { $diagnostic }
task-record-unavailable-reason = 사용 불가 원인: { $reason }
task-record-task-diagnostic = 작업 진단: `{ $code }`; 원인 { $reason }
task-record-rejection-reason = { $code ->
    [missing] 모델 출력 누락
    [duplicate] 모델 출력 중복
    [invalid_shape] { $detail }
    [invalid_shape_array] 번역은 문자열 배열이어야 합니다
    [invalid_shape_item] 번역 배열의 { $line }번째 항목은 문자열이어야 합니다
    [line_count_mismatch] 줄 수 불일치(예상 { $expected }, 실제 { $actual })
    [invalid_line_text] { $line }번째 줄에 잘못된 제어 문자가 있음
    [blank_line_mismatch] { $line }번째 줄의 공백 상태 불일치(예상: { $expected_blank ->
        [blank] 공백
       *[other] 공백 아님
    })
    [blank_translation] 번역문이 비어 있음
    [no_natural_language_text] 번역문에 자연어 텍스트가 없음
    [contains_byte_order_mark] 번역문에 BOM이 포함됨
    [placeholder_mismatch] 자리표시자 불일치: { $detail }
    [unexpected_placeholder] 알 수 없는 자리표시자: { $detail }
    [placeholder_normalization_ambiguous] 자리표시자 정규화가 모호함: { $detail }
    [source_residual] 원문 언어 잔류 감지: { $detail }
    [tag_value_contains_closing_delimiter] { $line }번째 줄에 태그 값을 조기에 닫는 '>'가 포함되어 있습니다
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason이 stop이 아님: { $detail }
    [invalid_response] { $detail }
    [invalid_id] 모델의 { $index }번째 항목 ID가 잘못됨
    [unknown_id] 모델의 { $index }번째 항목이 알 수 없는 ID { $detail }을 반환함
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] 모델 응답을 구문 분석할 수 없음
    [all_outputs_rejected] 모든 모델 출력이 검수에서 거부됨
    [recoverable_request_exhausted] 복구 가능한 요청의 재시도 예산 소진
    [retry_after_exceeds_maximum] Retry-After가 설정된 최대 대기 시간을 초과함
   *[other] { $code }
}
task-record-duration-seconds = { $value }초
task-record-duration-milliseconds = { $value }밀리초
