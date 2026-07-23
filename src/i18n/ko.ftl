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
    [process_output] 프로세스 출력
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
