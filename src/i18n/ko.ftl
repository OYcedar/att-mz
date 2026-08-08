app-about = 재사용 가능한 프로젝트 상태로 게임과 구조화된 텍스트를 번역합니다
cli-ui-language-help = 도움말, 진단, 진행률, 결과 및 프로젝트 로그의 언어: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko 또는 vi
cli-mz-about = RPG Maker MZ 게임 번역
cli-mv-about = RPG Maker MV 게임 번역
cli-generic-about = 규정된 JSONL 텍스트 번역
cli-init-about = 이름이 지정된 번역 프로젝트 초기화 또는 업데이트
cli-extract-about = 프로젝트의 현재 입력에서 원문 동기화
cli-translate-about = 명시적 또는 저장된 Profile로 추출된 원문 번역
cli-write-back-about = 현재 번역을 프로젝트 출력에 쓰기
cli-manual-about = 편집 가능한 TOML 파일로 수동 번역 관리
cli-manual-export-about = 현재 수동 번역이 필요한 항목 내보내기
cli-manual-check-about = 프로젝트를 변경하지 않고 수동 번역 TOML 검사
cli-manual-apply-about = 입력 완료된 유효한 수동 번역 적용
cli-project-lua-about = 프로젝트 데이터베이스에서 Lua 스크립트 실행
cli-project-name-help = 안정적인 프로젝트 이름
cli-init-path-help = 입력 루트 디렉터리. 기존 프로젝트는 마지막 성공 경로를 재사용할 수 있습니다
cli-source-language-help = 원문 언어 ID
cli-target-language-help = 대상 언어 ID
cli-dialogue-width-help = 대화 줄당 최대 전각 문자 수
cli-scrolling-width-help = 스크롤 텍스트 줄당 최대 전각 문자 수
cli-help-width-help = 도움말 또는 설명 줄당 최대 전각 문자 수
cli-builtin-help = ATT 내장 RPG Maker 텍스트 위치 사용
cli-rules-help = 이 TOML 정의로 RPG Maker 추출 규칙 교체. 빈 규칙 목록은 규칙을 비활성화합니다
cli-dialogue-rules-help = Builtin과 함께 쓰는 MV 대화 이름 투영 교체
cli-profile-help = 번역 Profile ID. 생략하면 마지막 성공 Profile을 재사용합니다
cli-terms-help = 프로젝트 용어 리소스 교체
cli-placeholders-help = 프로젝트 Placeholder 리소스 교체
cli-project-lua-script-help = 프로젝트 데이터베이스에서 실행할 Lua 스크립트
cli-project-lua-arguments-help = -- 뒤에서 Lua arg[1..]에 전달할 UTF-8 인수
cli-manual-file-help = 수동 번역 TOML 파일
cli-usage-heading = 사용법:
cli-commands-heading = 명령:
cli-options-heading = 옵션:
cli-arguments-heading = 인수:
cli-options-metavar = 옵션
cli-command-metavar = 명령
cli-print-help = 도움말 출력
cli-print-version = 버전 출력
cli-blank-value = 값은 비워 둘 수 없습니다.
cli-invalid-positive-integer = 값은 양의 정수여야 합니다.
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
error-no-executable-extract-owner = 지운 뒤 실행 가능한 Extract owner가 없어 계획을 저장하지 않았습니다.
plan-source-explicit = 명시적 입력
plan-source-project-state = 프로젝트 상태
plan-source-product-default = 제품 동작
notice-init-reuse-path = 원본 경로가 없어 마지막 성공 경로를 재사용합니다: { $path }.
notice-extract-reuse-owners = 추출 범위가 없어 마지막 성공 계획을 재사용합니다: { $owners }.
notice-translate-reuse-profile = Profile이 없어 마지막 성공 Profile을 재사용합니다: { $profile }.
notice-owner-disabled = owner { $owner }을 비활성화하고 이후 자동 계획에서 제거했습니다.
warning-rules-command-non-string-skipped = 경고: Rules 규칙 { $rule_number }에서 문자열이 아닌 command 매개변수 { $skipped_count }개를 건너뛰었습니다(소스 { $source_file }, code={ $command_code }, parameter={ $parameter }, 유형 { $actual_type }).
warning-manual-layout-required = 경고: { $locations }의 줄바꿈을 수동으로 확인해야 합니다(region={ $region }, max_fullwidth_chars={ $max_fullwidth_chars }).
notice-no-model-request = 모든 번역 단위가 최신 상태여서 이번 실행에서는 모델 요청을 보내지 않았습니다.
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
progress-extract-commit = 추출 자산 커밋 중
progress-generic-init = Generic 프로젝트 초기화 중
progress-generic-extract = Generic JSONL 입력 검색 중
progress-translate-planning = 번역 작업 계획 중
progress-translate-confirmed = 확인된 번역 작업
progress-translate-no-work = 모델 요청이 필요하지 않음
progress-project-lua = 프로젝트 Lua 프로그램 실행 중
progress-write-back-read-assets = 승인된 자산 읽는 중
progress-write-back-planning = 문서 다시 쓰기 계획 중
progress-write-back-documents = 문서 다시 쓰기
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
result-translate-summary = 번역: 작업 { $total }, 완료 { $complete }, 부분 { $partial }, 사용 불가 { $unavailable }; { $written }개 위치 기록, { $remaining }개 남음
result-translate-convergence = 상태 수렴: 유지 { $retained }, 무효화 { $invalidated }, 해당 없음 { $not_applicable }, 재사용 { $reused }
result-write-back-completed = 쓰기 완료: { $project }
result-project-lua-completed = 프로젝트 Lua 실행 완료: { $project }
result-output-directory = 출력 디렉터리: { $path }
result-write-back-summary = 쓰기: 번역 { $translated }단위, 원문 { $original }단위; 자동 줄바꿈 { $auto_wrapped }, 줄바꿈 추가 { $breaks }, 전각 들여쓰기 추가 { $indents }; 수동 배치 { $manual }
result-generic-extract-unchanged = Generic 입력 변경 없음: 파일 { $files }개, 그룹 { $groups }개, 단위 { $units }개
result-generic-extract-updated = Generic 입력 갱신: 파일 { $files }개, 그룹 { $groups }개, 단위 { $units }개; 번역 { $preserved }개 유지, { $cleared }개 삭제
result-generic-translate-summary = Generic 번역: 작업 { $total }, 완료 { $complete }, 부분 { $partial }, 사용 불가 { $unavailable }; 초기화 { $cleared }, 재사용 { $reused }, 수락 { $accepted }, 기록 { $written }, 충돌 { $conflicted }, 응답 문제 { $problems }
result-generic-write-back-summary = Generic 쓰기: 번역 { $translated }단위, 원문 유지 { $original }단위
result-symbol-repair-summary = 기호 복구: { $attempted }개 단위 시도, { $repaired }개 복구, 내부 건너뜀 { $skipped }개, 기호 { $replacements }개 교체
result-cancelled = 안전한 마무리 후 명령을 취소했습니다.
result-plan-saved = 성공한 실행 계획을 저장했습니다.
log-run-started = 명령 { $command }이 시작되었습니다.
log-run-succeeded = 명령 { $command }이 성공적으로 완료되었습니다.
log-run-failed = 명령 { $command }이 실패했습니다.
log-run-outcome-unknown = 명령 { $command }이 종료되었지만 최종 결과를 알 수 없습니다. 오류에 표시된 복구 위치를 따르십시오.
log-run-cancelled = 명령 { $command }이 취소되었습니다.
log-performance-counters = 성능 카운터: SQLite 트랜잭션 제어 시도 { $sqlite_control_attempted_total }회, 전체 후보 트리 검증 시작 { $candidate_validation_started }회, 완료 { $candidate_validation_completed }회.
log-lua-print = Lua: { $message }
log-plan-resolved = 명령 { $command }의 계획 출처: { $source }.
log-phase-started = 단계 시작: { $phase }.
log-retry-summary = { $count }회 재시도했습니다.
log-translation-task-started = 번역 작업 { $index }/{ $total } 시작.
log-translation-task-finished = 번역 작업 { $index }이 결과 { $outcome }으로 종료되었습니다.
log-run-recovery-required = 명령 { $command }이 복구가 필요한 상태로 끝났습니다. 진단에 표시된 복구 위치를 확인하십시오.
log-phase-completed = 단계 완료: { $phase }.
log-phase-stopped = { $outcome ->
    [failed] 단계 실패: { $phase }.
    [cancelled] 단계 취소됨: { $phase }.
   *[other] 단계 중지됨: { $phase }.
}
log-cancellation-requested = { $total }개 중 { $confirmed }개를 확인한 뒤 취소가 요청되었습니다.
log-cancellation-requested-indeterminate = { $confirmed }개를 확인한 뒤 취소가 요청되었습니다. 전체 개수는 알 수 없습니다.
log-run-plan-finalized = { $result ->
    [saved] 실행 계획을 저장했습니다.
    [not_saved] 실행 계획을 저장하지 못했습니다.
    [saved_finalization_failed] 실행 계획은 저장했지만 마무리 작업이 실패했습니다.
    [outcome_unknown] 실행 계획의 최종 상태를 알 수 없습니다.
   *[other] 실행 계획 마무리가 알 수 없는 결과로 중지되었습니다.
}
log-translation-finished = { $result ->
    [not_started] 번역이 시작되지 않았습니다.
    [no_work] 번역할 내용이 없어 종료되었습니다.
    [complete] 번역이 완료되었습니다.
    [incomplete] 완료되지 않은 작업이 남은 채 번역이 종료되었습니다.
    [failed] 번역이 실패했습니다.
    [cancelled] 번역이 취소되었습니다.
   *[other] 번역이 알 수 없는 결과로 중지되었습니다.
}
log-publication-started = 출력 루트 { $path }에 게시를 시작했습니다.
log-publication-finished = { $result ->
    [published] 게시가 완료되었습니다.
    [not_published] 게시가 출력을 변경하지 않았습니다.
    [recovery_required] 게시가 중지되었으며 복구가 필요합니다.
    [outcome_unknown] 게시의 최종 상태를 알 수 없습니다.
   *[other] 게시가 알 수 없는 결과로 중지되었습니다.
}
log-project-log-degraded = 프로젝트 로그에 문제가 발생하여 { $failure_kinds }개 장애 범주를 기록했습니다.
log-task-outcome-value = { $outcome ->
    [complete] 완료
    [partial] 일부 완료
    [unavailable] 사용 불가
    [failed] 실패
    [not_committed_after_earlier_failure] 이전 실패로 미커밋
    [cancelled] 취소됨
   *[other] 알 수 없는 결과로 종료
}
diagnostic-location = 위치: { $subject }
diagnostic-explanation = 원인: { $reason }
diagnostic-resolution = 조치: { $action }
diagnostic-related = 관련 오류 { $index }:
diagnostic-resolution-value = { $code ->
    [fix_configuration] 표시된 구성 필드를 수정한 후 다시 시도하세요
    [fix_input] 표시된 입력을 수정한 후 다시 시도하세요
    [fix_placeholder_rules] 표시된 Placeholder 규칙을 수정한 후 다시 시도하세요
    [adjust_manual_layout] 표시된 위치와 표시 너비에 맞게 줄바꿈과 레이아웃을 수동으로 조정하세요
    [check_path_and_permissions] 경로, 파일 시스템 상태 및 권한을 확인하세요
    [check_project_state] 프로젝트 상태를 확인하고 수정한 후 다시 시도하세요
    [resolve_contention] 충돌하는 작업이 끝날 때까지 기다린 후 다시 시도하세요
    [check_model_service] 모델 서비스 응답과 계정 한도를 확인하세요
    [preserve_recovery_artifacts] 나열된 복구 산출물을 삭제하지 말고, 출력을 복구한 후 다시 시도하세요
    [retry] 작업을 다시 시도하세요
    [report_bug] ATT 결함을 보고하고 당시 수행하던 작업을 설명하세요
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 필수 값이 없습니다
    [generic_extract_required] JSONL 입력이 최근 Extract와 일치하지 않습니다. att generic extract를 다시 실행하세요
    [conflicting_values] 제공한 값이 서로 충돌합니다
    [invalid_syntax] 값의 구문이 잘못되었습니다
    [invalid_encoding] 텍스트 인코딩이 잘못되었습니다
    [invalid_value] 값이 필수 계약을 위반합니다
    [not_found] 필요한 객체가 없습니다
    [state_mismatch] 저장된 프로젝트 상태가 이 작업의 요구 사항을 충족하지 않습니다
    [unsupported_windows_code_page] Windows 코드 페이지가 UTF-8이 아닙니다
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
    [lua_compilation_failed] Lua 주 프로그램을 컴파일할 수 없습니다
    [lua_execution_failed] Lua 주 프로그램 실행 중 오류가 발생했습니다
    [rules_pattern_match_failed] Rules PCRE2 패턴을 평가할 수 없습니다
    [rules_zero_width_match] Rules 패턴이 너비가 0인 일치를 만들었습니다
    [rules_overlapping_capture] Rules 패턴이 겹치는 텍스트 캡처를 만들었습니다
    [rules_missing_text_capture] 필수 명명 텍스트 캡처가 일치에 참여하지 않았습니다
    [rules_invalid_capture_range] Rules 일치 또는 캡처 범위가 유효한 UTF-8 문자 경계를 벗어났습니다
    [write_back_candidate_invalid] 쓰기 반영 후보가 필수 data/js 트리 구조를 충족하지 않습니다
    [write_back_recovery_required] 내용을 신뢰하기 전에 출력 디렉터리를 복구해야 합니다
    [already_exists] 대상 개체가 이미 존재합니다
    [cancelled] 작업이 취소되었습니다
    [concurrent_modification] 프로젝트 상태가 동시에 변경되었습니다
    [duplicate_identifier] 식별자가 중복되었습니다
    [extraction_out_of_date] 저장된 추출 결과가 현재 원본과 더 이상 일치하지 않습니다
    [invalid_content] 내용이 필수 계약을 위반합니다
    [manual_layout_required] 줄 바꿈 또는 레이아웃을 수동으로 조정해야 합니다
    [operation_failed] 작업에 실패했습니다
    [placeholder_projection_failed] Placeholder 투영이 필수 구조를 보존하지 못했습니다
    [profile_not_found] 선택한 번역 Profile이 존재하지 않습니다
    [recovery_required] 결과를 신뢰하려면 먼저 복구해야 합니다
    [resource_limit] 필요한 리소스 한도에 도달했습니다
    [resource_limit_exceeded] 작업이 백엔드 리소스 한도를 초과했습니다
    [source_snapshot_mismatch] 원본이 저장된 스냅샷과 더 이상 일치하지 않습니다
    [unavailable] 요청한 작업을 일시적으로 사용할 수 없습니다
    [internal_invariant] 내부 불변 조건을 위반했습니다. ATT 결함입니다
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] 언어 정책 용어는 비워 둘 수 없습니다
    [language_policy_term_surrounding_whitespace] 언어 정책 용어 앞뒤에 공백을 둘 수 없습니다
    [language_policy_term_duplicate] 언어 정책 용어는 중복될 수 없습니다
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
diagnostic-http-status = HTTP 상태 { $status }
diagnostic-retry-after = Retry-After: { $seconds }초
diagnostic-provider-code = 공급자 code: { $code }
diagnostic-provider-type = 공급자 type: { $kind }
diagnostic-provider-message = 공급자 메시지: { $message }
diagnostic-json-position = { $line }행 { $column }열
diagnostic-placeholder-rule-file = { $path }의 Placeholder 규칙 { $number }
diagnostic-placeholder-rule-project = 현재 프로젝트의 Placeholder 규칙 { $number }
manual-exported = { $entries }개 항목을 { $path }에 내보냈습니다
manual-checked = 유효 { $valid }, 미입력 { $unfilled }, 오류 { $errors }
manual-applied = 적용 { $applied }, 미입력 { $unfilled }, 오류 { $errors }
manual-issue = { $object }: { $reason }; { $help }.
manual-value = { $code ->
    [invalid_source_line] source의 { $line }번째 항목에 줄바꿈 또는 NUL이 있습니다
    [invalid_translation_line] translation의 { $line }번째 항목에 줄바꿈 또는 NUL이 있습니다
    [fixed_length] fixed 번역은 { $expected }개 항목이 필요하지만 { $actual }개입니다
    [fixed_blank_slot] fixed 번역의 { $line }번째 항목은 비워 두어야 합니다
    [rerun_export] manual export를 다시 실행하세요
    [rerun_export_without_controls] manual export를 다시 실행하고 배열 항목에 줄바꿈이나 NUL을 넣지 마세요
    [rerun_export_then_fill] manual export를 다시 실행한 뒤 번역을 입력하세요
    [resolve_temporary_then_rerun_export] 표시된 고정 임시 경로를 확인하고 남은 객체가 있으면 제거한 다음 manual export를 다시 실행하세요
    [keep_exported_type] manual export가 기록한 type을 유지하세요
   *[other] __ATT_FALLBACK__
}
task-record-title = 번역 작업
task-record-final-result-heading = 최종 결과
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
task-record-task-diagnostic = 작업 진단
