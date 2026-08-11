app-about = Dịch trò chơi và văn bản có cấu trúc với trạng thái dự án có thể tái sử dụng
cli-ui-language-help = Ngôn ngữ cho trợ giúp, chẩn đoán, tiến độ, kết quả và nhật ký dự án: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko hoặc vi
cli-mz-about = Dịch trò chơi RPG Maker MZ
cli-mv-about = Dịch trò chơi RPG Maker MV
cli-generic-about = Dịch văn bản JSONL có cấu trúc
cli-init-about = Khởi tạo hoặc cập nhật dự án dịch có tên
cli-extract-about = Đồng bộ văn bản nguồn từ đầu vào hiện tại của dự án
cli-translate-about = Dịch văn bản đã trích xuất bằng Profile đã chỉ định hoặc đã lưu
cli-write-back-about = Ghi bản dịch hiện tại vào đầu ra của dự án
cli-manual-about = Quản lý bản dịch thủ công trong tệp TOML có thể chỉnh sửa
cli-manual-export-about = Xuất các mục hiện cần dịch thủ công
cli-ownership-export-about = Xuất quyền sở hữu văn bản của mọi đơn vị RPG Maker đã trích xuất
cli-translation-export-about = Xuất văn bản nguồn, bản dịch hiện tại và trạng thái của mọi đơn vị đã trích xuất
cli-manual-check-about = Kiểm tra TOML bản dịch thủ công mà không thay đổi dự án
cli-manual-apply-about = Áp dụng các bản dịch thủ công đã điền và hợp lệ
cli-project-lua-about = Chạy tập lệnh Lua trên cơ sở dữ liệu dự án
cli-project-name-help = Tên dự án ổn định
cli-init-path-help = Thư mục gốc đầu vào; dự án hiện có có thể dùng lại đường dẫn thành công gần nhất
cli-source-language-help = ID ngôn ngữ nguồn
cli-target-language-help = ID ngôn ngữ đích
cli-builtin-help = Dùng các vị trí văn bản RPG Maker tích hợp của ATT
cli-rules-help = Thay quy tắc trích xuất RPG Maker bằng định nghĩa TOML này; danh sách rỗng sẽ tắt quy tắc
cli-dialogue-rules-help = Thay phép chiếu tên hội thoại MV dùng cùng Builtin
cli-profile-help = ID Profile dịch; bỏ qua để dùng lại Profile thành công gần nhất
cli-terms-help = Thay tài nguyên thuật ngữ của dự án
cli-placeholders-help = Thay tài nguyên Placeholder của dự án
cli-project-lua-script-help = Tập lệnh Lua sẽ chạy trên cơ sở dữ liệu dự án
cli-project-lua-arguments-help = Đối số UTF-8 truyền cho Lua arg[1..] sau --
cli-manual-file-help = Tệp TOML bản dịch thủ công
cli-usage-heading = Cách dùng:
cli-commands-heading = Lệnh:
cli-options-heading = Tùy chọn:
cli-arguments-heading = Đối số:
cli-options-metavar = TÙY_CHỌN
cli-command-metavar = LỆNH
cli-print-help = In trợ giúp
cli-print-version = In phiên bản
cli-blank-value = Giá trị không được để trống.
cli-invalid-positive-integer = Giá trị phải là số nguyên dương.
cli-invalid-ui-language-argument = --ui-language chứa thẻ ngôn ngữ không hợp lệ: { $value }.
cli-unsupported-ui-language-argument = --ui-language yêu cầu ngôn ngữ không được hỗ trợ: { $value }.
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE chứa thẻ ngôn ngữ không hợp lệ: { $value }.
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE yêu cầu ngôn ngữ không được hỗ trợ: { $value }.
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE không phải Unicode hợp lệ.
cli-unexpected-argument = Đối số không mong đợi: { $value }.
cli-missing-required-argument = Thiếu đối số bắt buộc: { $value }.
cli-invalid-value = Giá trị { $value } không hợp lệ cho { $argument }.
cli-error-heading = Lỗi:
cli-try-help = Để biết thêm thông tin, hãy dùng --help.
cli-missing-value = Cần cung cấp giá trị cho { $argument }.
cli-missing-subcommand = Cần cung cấp một lệnh.
cli-argument-conflict = Không thể dùng { $argument } cùng các đối số đã cung cấp khác.
cli-wrong-number-of-values = Số lượng giá trị cho { $argument } không đúng.
cli-invalid-utf8 = Một đối số dòng lệnh không phải Unicode hợp lệ.
cli-parse-failure = Không thể phân tích dòng lệnh.
plan-source-explicit = đầu vào chỉ định
plan-source-project-state = trạng thái dự án
plan-source-product-default = hành vi sản phẩm
notice-init-reuse-path = Không có đường dẫn nguồn; đang dùng lại đường dẫn thành công gần nhất: { $path }.
notice-extract-reuse-owners = Không có phạm vi trích xuất; đang dùng lại kế hoạch thành công gần nhất: { $owners }.
notice-translate-reuse-profile = Không có Profile; đang dùng lại Profile thành công gần nhất: { $profile }.
notice-no-model-request = Mọi đơn vị dịch đều mới nhất; lần chạy này không cần gửi yêu cầu nào đến mô hình.
progress-init-check-project = Đang kiểm tra trạng thái dự án
progress-init-scan-source = Đang quét nguồn trò chơi
progress-init-build-candidate = Đang dựng ứng viên dự án
progress-init-converge-database = Đang hội tụ cơ sở dữ liệu dự án
progress-init-publish = Đang xuất bản dự án đã khởi tạo
progress-save-run-plan = Đang lưu kế hoạch chạy thành công
progress-extract-owner = Owner trích xuất: { $owner }
progress-extract-documents = Đang quét tài liệu
progress-extract-builtin = Đơn vị công việc Builtin
progress-extract-rules = Định nghĩa Rules
progress-extract-commit = Đang commit tài sản đã trích xuất
progress-generic-init = Đang khởi tạo dự án Generic
progress-generic-extract = Đang quét đầu vào Generic JSONL
progress-translate-planning = Đang lập kế hoạch tác vụ dịch
progress-translate-confirmed = Tác vụ dịch đã xác nhận
progress-no-work = Không có nội dung cần xử lý
progress-project-lua = Đang chạy chương trình Lua của dự án
progress-write-back-read-assets = Đang đọc tài sản đã duyệt
progress-write-back-planning = Đang lập kế hoạch viết lại tài liệu
progress-write-back-documents = Đã viết lại tài liệu
progress-write-back-validate-candidate = Đang xác thực ứng viên đầu ra
progress-write-back-publish = Đang xuất bản đầu ra; khi ngắt vẫn chờ kết quả được xác nhận
progress-finalizing = Đang hoàn tất tài nguyên bắt buộc
progress-safe-stopping = Đang dừng an toàn; giữ lại tiến độ đã xác nhận gần nhất
result-init-completed = Khởi tạo hoàn tất: { $project }
result-init-created = Trạng thái dự án: đã tạo
result-init-unchanged = Trạng thái dự án: không đổi
result-init-updated = Trạng thái dự án: đã cập nhật
result-init-stale-owners = Cần trích xuất lại: { $owners }
result-extract-completed = Trích xuất hoàn tất: { $project }
result-translate-completed = Lần chạy dịch đã kết thúc: { $project } (Profile: { $profile })
result-translate-status = Trạng thái: { $status }
result-translate-status-value = { $status ->
    [no_work] không cần xử lý
    [complete] đầy đủ
    [incomplete] chưa đầy đủ
   *[other] __ATT_FALLBACK__
}
result-translate-summary = Dịch: { $total } tác vụ đã lên kế hoạch, { $started } đã bắt đầu, { $not_started } chưa bắt đầu; { $complete } hoàn tất, { $partial } một phần, { $unavailable } không khả dụng, { $failed } thất bại, { $cancelled } đã hủy; đã ghi { $written } vị trí, còn { $remaining }
result-translate-convergence = Hội tụ trạng thái: giữ { $retained }, vô hiệu { $invalidated }, không áp dụng { $not_applicable }, tái dùng { $reused }
result-write-back-completed = Ghi lại hoàn tất: { $project }
result-project-lua-completed = Thực thi Lua dự án hoàn tất: { $project }
result-output-directory = Thư mục đầu ra: { $path }
result-write-back-summary = Ghi lại: { $translated } đơn vị dịch, { $original } đơn vị nguồn
result-generic-extract-unchanged = Đầu vào Generic không đổi: { $files } tệp, { $groups } nhóm, { $units } đơn vị
result-generic-extract-updated = Đã cập nhật đầu vào Generic: { $files } tệp, { $groups } nhóm, { $units } đơn vị; giữ { $preserved } bản dịch và xóa { $cleared }
result-generic-translate-summary = Dịch Generic: { $total } tác vụ đã lên kế hoạch, { $started } đã bắt đầu, { $not_started } chưa bắt đầu; { $complete } hoàn tất, { $partial } một phần, { $unavailable } không khả dụng, { $failed } thất bại, { $cancelled } đã hủy; { $planned_units } Unit đã lên kế hoạch, còn { $remaining_units } Unit, xóa { $cleared }, dùng lại { $reused }, chấp nhận { $accepted }, ghi { $written }, xung đột { $conflicted }, lỗi phản hồi { $problems }
result-generic-write-back-summary = Ghi lại Generic: { $translated } đơn vị dịch, giữ nguyên { $original } đơn vị nguồn
result-run-log = Nhật ký lần chạy: { $path }
translate-incomplete-object = Lần chạy Translate của dự án { $project }
translate-incomplete-rpg-maker-reason = { $partial } tác vụ một phần, { $unavailable } tác vụ không khả dụng, { $not_started } chưa bắt đầu, { $protocol } lỗi giao thức và { $exhausted } yêu cầu đã cạn; nhận yêu cầu {
    $admission ->
        [stopped] đã dừng
       *[open] vẫn mở
    }; còn { $remaining_decisions } quyết định và { $remaining_locations } vị trí
translate-incomplete-generic-reason = { $partial } tác vụ một phần, { $unavailable } tác vụ không khả dụng, { $not_started } chưa bắt đầu, { $exhausted } yêu cầu đã cạn; nhận yêu cầu {
    $admission ->
        [stopped] đã dừng
       *[open] vẫn mở
    }; còn { $remaining_units } Unit, { $conflicted } xung đột ghi và { $problems } lỗi phản hồi
translate-incomplete-help = Xem chẩn đoán tác vụ trong nhật ký lần chạy này, sửa lỗi có thể lặp lại rồi chạy Translate lần nữa; dùng Manual nếu chỉ còn ít nội dung
result-cancelled = Lệnh đã bị hủy sau khi hoàn tất an toàn.
result-plan-saved = Kế hoạch chạy thành công đã được lưu.
log-run-started = Lệnh { $command } đã bắt đầu.
log-run-succeeded = Lệnh { $command } đã hoàn tất thành công.
log-run-failed = Lệnh { $command } thất bại.
log-run-outcome-unknown = Lệnh { $command } đã kết thúc nhưng kết quả cuối cùng chưa xác định; hãy làm theo các vị trí khôi phục trong lỗi.
log-run-cancelled = Lệnh { $command } đã bị hủy.
log-performance-counters = Bộ đếm hiệu năng: số lần thử điều khiển giao dịch SQLite { $sqlite_control_attempted_total }; xác thực toàn bộ cây ứng viên đã bắt đầu { $candidate_validation_started }, đã hoàn tất { $candidate_validation_completed }.
log-lua-print = Lua: { $message }
log-plan-resolved = Lệnh { $command } lấy kế hoạch từ { $source }.
log-phase-started = Bắt đầu giai đoạn: { $phase }.
log-retry-summary = Đã thực hiện { $count } lần thử lại.
log-translation-task-started = Tác vụ dịch { $index }/{ $total } đã bắt đầu.
log-translation-task-finished = Tác vụ dịch { $index } kết thúc với kết quả { $outcome }.
log-run-recovery-required = Lệnh { $command } kết thúc ở trạng thái cần khôi phục; hãy làm theo các vị trí khôi phục trong chẩn đoán.
log-phase-completed = Giai đoạn đã hoàn tất: { $phase }.
log-phase-stopped = { $outcome ->
    [failed] Giai đoạn thất bại: { $phase }.
    [cancelled] Giai đoạn đã bị hủy: { $phase }.
   *[other] Giai đoạn đã dừng: { $phase }.
}
log-cancellation-requested = Đã yêu cầu hủy sau khi xác nhận { $confirmed } trên { $total } mục.
log-cancellation-requested-indeterminate = Đã yêu cầu hủy sau khi xác nhận { $confirmed } mục; chưa biết tổng số.
log-run-plan-finalized = { $result ->
    [saved] Đã lưu kế hoạch chạy.
    [not_saved] Chưa lưu kế hoạch chạy.
    [saved_finalization_failed] Đã lưu kế hoạch chạy nhưng bước hoàn tất thất bại.
    [outcome_unknown] Chưa biết trạng thái cuối của kế hoạch chạy.
   *[other] Bước hoàn tất kế hoạch dừng với kết quả không xác định.
}
log-translation-finished = { $result ->
    [not_started] Bản dịch chưa bắt đầu.
    [no_work] Bản dịch kết thúc vì không có nội dung cần xử lý.
    [complete] Bản dịch đã hoàn tất.
    [incomplete] Bản dịch kết thúc nhưng vẫn còn phần chưa hoàn tất.
    [failed] Bản dịch thất bại.
    [cancelled] Bản dịch đã bị hủy.
   *[other] Bản dịch dừng với kết quả không xác định.
}
log-publication-started = Đã bắt đầu xuất bản vào thư mục gốc { $path }.
log-publication-finished = { $result ->
    [published] Xuất bản đã hoàn tất.
    [not_published] Xuất bản không thay đổi đầu ra.
    [recovery_required] Xuất bản đã dừng và cần khôi phục.
    [outcome_unknown] Chưa biết trạng thái cuối của việc xuất bản.
   *[other] Xuất bản dừng với kết quả không xác định.
}
log-task-outcome-value = { $outcome ->
    [complete] hoàn tất
    [partial] hoàn tất một phần
    [unavailable] không khả dụng
    [failed] thất bại
    [not_committed_after_earlier_failure] chưa commit do lỗi trước đó
    [cancelled] đã hủy
   *[other] kết thúc với kết quả không xác định
}
diagnostic-object = Đối tượng: { $subject }
diagnostic-error-heading = Lỗi:
diagnostic-warning-heading = Cảnh báo:
diagnostic-explanation = Nguyên nhân: { $reason }
diagnostic-impact = Ảnh hưởng: { $impact }
diagnostic-resolution = Cách xử lý: { $action }
diagnostic-related = { $relation ->
    [cleanup] Việc dọn dẹp cũng thất bại:
    [rollback] Việc hoàn tác cũng thất bại:
    [discard] Việc loại bỏ bản ứng viên cũng thất bại:
    [finalization] Việc hoàn tất cũng thất bại:
    [shutdown] Việc đóng cũng thất bại:
    [observability] Việc hiển thị hoặc ghi kết quả cũng thất bại:
   *[other] Một thao tác liên quan cũng thất bại:
}
diagnostic-impact-value = { $effect ->
    [unchanged] Trạng thái nghiệp vụ không thay đổi
    [progress_preserved] Tiến độ đã xác nhận trước đó được giữ lại; nội dung được chỉ ra chưa hoàn tất
    [applied] Kết quả nghiệp vụ liên quan đã có hiệu lực
    [applied_run_plan_not_saved] Kết quả nghiệp vụ đã có hiệu lực nhưng kế hoạch lần chạy này chưa được lưu
    [applied_finalization_failed] Kết quả nghiệp vụ đã có hiệu lực nhưng bước hoàn tất bắt buộc chưa xong
    [recovery_required] Kết quả đã rõ nhưng phải xử lý vị trí khôi phục được chỉ ra trước
    [outcome_unknown] Không thể xác nhận thao tác đã có hiệu lực hay chưa; đừng thử lại hoặc xóa hiện trường khôi phục trước khi làm theo cách xử lý
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] Sửa trường cấu hình được nêu rồi thử lại
    [fix_input] Sửa dữ liệu đầu vào được nêu rồi thử lại
    [fix_placeholder_rules] Sửa quy tắc Placeholder được nêu rồi thử lại
    [review_disabled_rules] Nếu đây là kết quả mong đợi thì không cần xử lý; nếu không, hãy thêm quy tắc hợp lệ vào tệp được chỉ ra rồi chạy lại Extract
    [check_path_and_permissions] Kiểm tra đường dẫn, trạng thái hệ thống tệp và quyền
    [check_project_state] Kiểm tra và sửa trạng thái dự án rồi thử lại
    [resolve_contention] Chờ thao tác xung đột kết thúc rồi thử lại
    [check_model_service] Kiểm tra phản hồi của dịch vụ mô hình và giới hạn tài khoản
    [preserve_recovery_artifacts] Không xóa các tạo phẩm khôi phục được liệt kê; hãy khôi phục đầu ra trước khi thử lại
    [retry] Thử lại thao tác
    [report_bug] Báo cáo lỗi ATT này và mô tả thao tác đang thực hiện
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Thiếu một giá trị bắt buộc
    [generic_extract_required] Dữ liệu JSONL không còn khớp với lần Extract gần nhất; hãy chạy lại att generic extract
    [conflicting_values] Các giá trị được cung cấp xung đột với nhau
    [invalid_syntax] Cú pháp của giá trị không hợp lệ
    [invalid_encoding] Mã hóa văn bản không hợp lệ
    [invalid_value] Giá trị vi phạm hợp đồng bắt buộc
    [empty_text_capture] Phần bắt text trống
    [rules_owner_disabled] Tệp Rules đã chọn dùng rule = []; Rules đã bị tắt và các tài nguyên trích xuất của nó đã bị xóa
    [not_found] Đối tượng bắt buộc không tồn tại
    [state_mismatch] Trạng thái dự án đã lưu không đáp ứng thao tác này
    [unsupported_windows_code_page] Bảng mã Windows không phải UTF-8
    [transaction_rolled_back] Giao dịch thất bại và các thay đổi đã được hoàn tác
    [transaction_outcome_unknown] Giao dịch kết thúc mà không xác nhận được commit hay hoàn tác
    [finalization_failed] Kết quả thao tác đã tồn tại nhưng bước hoàn tất thất bại
    [rollback_failed] Cả thao tác chính và hoàn tác đều thất bại
    [external_service_rejected] Dịch vụ bên ngoài đã từ chối yêu cầu
    [external_service_unavailable] Dịch vụ bên ngoài không khả dụng
    [executor_closed] Dịch vụ thực thi đang đóng hoặc đã đóng
    [concurrent_shutdown] Một bên gọi khác đang đóng bộ thực thi
    [executor_state_poisoned] Trạng thái vòng đời của bộ thực thi đã bị hỏng
    [worker_spawn_failed] Hệ điều hành không thể tạo luồng worker
    [stdout_write_failed] Không thể ghi vào đầu ra chuẩn
    [stderr_write_failed] Không thể ghi vào đầu ra lỗi chuẩn
    [stdout_flush_failed] Không thể xả đầu ra chuẩn
    [stderr_flush_failed] Không thể xả đầu ra lỗi chuẩn
    [worker_channel_closed] Kênh lệnh worker đã đóng trước khi hoàn tất
    [worker_panicked] Một worker kết thúc ngoài dự kiến
    [reparse_point_forbidden] Đường dẫn chứa điểm phân tích lại không đáng tin cậy
    [non_local_volume] Đường dẫn không nằm trên ổ đĩa cố định cục bộ
    [non_ntfs_volume] Đường dẫn không nằm trên ổ đĩa NTFS
    [case_sensitive_directory] Thư mục dùng ngữ nghĩa tên phân biệt chữ hoa chữ thường
    [lock_cancelled] Việc chờ khóa bắt buộc đã bị hủy
    [target_already_exists] Đích đã tồn tại
    [file_identity_changed] Danh tính tệp đã thay đổi trong khi thao tác
    [invalid_path] Đường dẫn không phải đích hợp lệ cho thao tác này
    [not_regular_file] Đích hiện có không phải là tệp thông thường
    [wrong_publisher_instance] Token phát hành thuộc về một phiên bản bộ phát hành khác
    [journal_corrupt] Nhật ký khôi phục phát hành không hợp lệ hoặc chưa hoàn chỉnh
    [unexpected_artifact] Tạo phẩm hệ thống tệp ngoài dự kiến đang chặn thao tác
    [interactive_session_already_open] Một phiên SQLite tương tác khác đang hoạt động
    [backup_incomplete] Bản sao lưu SQLite chưa đạt trạng thái hoàn tất
    [request_serialization_failed] Không thể tuần tự hóa yêu cầu mô hình
    [response_parsing_failed] Phản hồi mô hình không phải JSON hợp lệ
    [invalid_response_contract] Phản hồi mô hình không đáp ứng hợp đồng phản hồi bắt buộc
    [transport_failed] Truyền tải HTTP thất bại trước khi nhận được phản hồi hợp lệ
    [lua_compilation_failed] Không thể biên dịch chương trình Lua chính
    [lua_execution_failed] Chương trình Lua chính thất bại trong khi chạy
    [rules_pattern_match_failed] Không thể đánh giá mẫu PCRE2 của Rules
    [rules_zero_width_match] Mẫu Rules tạo ra kết quả khớp có độ rộng bằng không
    [rules_overlapping_capture] Mẫu Rules tạo ra các vùng bắt văn bản chồng lấp
    [rules_missing_text_capture] Vùng bắt văn bản có tên bắt buộc không tham gia kết quả khớp
    [rules_invalid_capture_range] Kết quả khớp hoặc vùng bắt Rules nằm ngoài ranh giới ký tự UTF-8 hợp lệ
    [write_back_candidate_invalid] Ứng viên ghi ngược không đáp ứng cấu trúc cây data/js bắt buộc
    [write_back_recovery_required] Cần khôi phục thư mục đầu ra trước khi có thể tin cậy nội dung
    [already_exists] Đối tượng đích đã tồn tại
    [cancelled] Thao tác đã bị hủy
    [concurrent_modification] Trạng thái dự án đã bị thay đổi đồng thời
    [duplicate_identifier] Mã định danh bị trùng lặp
    [extraction_out_of_date] Bản trích xuất đã lưu không còn khớp với nguồn hiện tại
    [invalid_content] Nội dung vi phạm hợp đồng bắt buộc
    [operation_failed] Thao tác thất bại
    [placeholder_projection_failed] Phép chiếu Placeholder không giữ nguyên cấu trúc bắt buộc
    [profile_not_found] Profile dịch đã chọn không tồn tại
    [recovery_required] Cần khôi phục trước khi có thể tin cậy kết quả
    [resource_limit] Đã đạt giới hạn tài nguyên bắt buộc
    [resource_limit_exceeded] Thao tác vượt quá giới hạn tài nguyên của dịch vụ
    [source_snapshot_mismatch] Nguồn không còn khớp với snapshot đã lưu
    [unavailable] Công việc được yêu cầu tạm thời không khả dụng
    [internal_invariant] Một bất biến nội bộ đã bị vi phạm; đây là lỗi của ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] Thuật ngữ chính sách ngôn ngữ không được để trống
    [language_policy_term_surrounding_whitespace] Thuật ngữ chính sách ngôn ngữ không được có khoảng trắng ở hai đầu
    [language_policy_term_duplicate] Thuật ngữ chính sách ngôn ngữ không được trùng lặp
    [language_id_blank] ID ngôn ngữ không được để trống
    [language_id_surrounding_whitespace] ID ngôn ngữ không được có khoảng trắng ở hai đầu
    [language_id_uses_underscore] ID ngôn ngữ phải dùng dấu gạch ngang giữa các thẻ con
    [language_id_invalid_syntax] ID ngôn ngữ phải đáp ứng cú pháp RFC 5646
    [language_id_invalid_registry_tag] ID ngôn ngữ chứa thẻ con registry không hợp lệ
    [language_id_canonicalization_failed] Không thể chuẩn hóa ID ngôn ngữ
    [language_id_undefined_primary_language] ID ngôn ngữ phải xác định ngôn ngữ chính
    [language_id_duplicate] ID ngôn ngữ phải là duy nhất
    [language_catalog_empty] Cần ít nhất một mô-đun ngôn ngữ nguồn
    [url_invalid] Giá trị phải là URL hợp lệ
    [url_credentials_forbidden] URL không được chứa thông tin xác thực
    [url_fragment_forbidden] URL không được chứa fragment
    [url_scheme_unsupported] Scheme URL phải là http hoặc https
    [api_key_blank] API key không được để trống
    [api_key_surrounding_whitespace] API key không được có khoảng trắng ở hai đầu
    [api_key_invalid_header] Không thể biểu diễn API key dưới dạng giá trị HTTP Header
    [strict_json_invalid] Giá trị phải là JSON nghiêm ngặt (dòng={ $line }, cột={ $column })
    [json_object_required] Giá trị phải là đối tượng JSON
    [reserved_request_field] Trường này thuộc sở hữu giao thức yêu cầu và không thể bị ghi đè
    [proxy_must_be_false_or_url] proxy phải là false hoặc URL http/https hoàn chỉnh
    [pem_path_duplicate] Đường dẫn PEM phải là duy nhất
    [runtime_maximum_exceeded] Giá trị vượt quá mức tối đa của thời gian chạy (thực tế={ $actual }, tối đa={ $maximum })
    [value_surrounding_whitespace] Giá trị không được có khoảng trắng ở hai đầu
    [value_blank] Giá trị không được để trống
    [path_blank] Đường dẫn không được để trống
    [positive_required] Giá trị phải lớn hơn không (thực tế={ $actual })
    [usize_range_exceeded] Giá trị vượt quá phạm vi usize của nền tảng này (thực tế={ $actual })
    [u32_range_exceeded] Giá trị vượt quá phạm vi u32 (thực tế={ $actual })
    [duplicate_profile_id] ID hồ sơ dịch phải là duy nhất
    [selected_profile_invalid] Cấu trúc hoặc kiểu trường của hồ sơ dịch đã chọn không hợp lệ
    [referenced_client_not_found] Máy khách LLM được tham chiếu không tồn tại
   *[other] __ATT_FALLBACK__
}
diagnostic-http-status = Trạng thái HTTP { $status }
diagnostic-retry-after = Retry-After: { $seconds } giây
diagnostic-provider-code = Mã nhà cung cấp: { $code }
diagnostic-provider-type = Loại nhà cung cấp: { $kind }
diagnostic-provider-message = Thông báo nhà cung cấp: { $message }
diagnostic-json-position = dòng { $line }, cột { $column }
diagnostic-placeholder-rule-file = Quy tắc Placeholder { $number } trong { $path }
diagnostic-placeholder-rule-project = Quy tắc Placeholder { $number } của dự án hiện tại
manual-exported = Đã xuất { $entries } mục vào { $path }
manual-checked = Hợp lệ { $valid }, chưa điền { $unfilled }, lỗi { $errors }
manual-applied = Đã áp dụng { $applied }, chưa điền { $unfilled }, lỗi { $errors }
manual-value = { $code ->
    [invalid_source_line] mục source { $line } chứa ký tự xuống dòng hoặc NUL
    [invalid_translation_line] mục translation { $line } chứa ký tự xuống dòng hoặc NUL
    [fixed_length] bản dịch fixed cần { $expected } mục; hiện có { $actual }
    [fixed_blank_slot] mục { $line } của bản dịch fixed phải để trống
    [rerun_export] Chạy lại manual export
    [rerun_export_without_controls] Chạy lại manual export và không đặt ký tự xuống dòng hoặc NUL trong các mục mảng
    [rerun_export_then_fill] Chạy lại manual export rồi điền bản dịch
    [resolve_temporary_then_rerun_export] Xử lý đường dẫn tạm thời cố định được hiển thị, xóa mọi đối tượng còn sót lại rồi chạy lại manual export
    [resolve_published_backup_cleanup] Cả hai tệp xuất đã có hiệu lực; hãy kiểm tra rồi xóa tệp backup cố định được hiển thị
    [keep_exported_type] Giữ nguyên type do manual export ghi ra
   *[other] __ATT_FALLBACK__
}
task-record-title = Tác vụ dịch
task-record-final-result-heading = Kết quả cuối
task-record-final-status = Trạng thái: { $state ->
    [complete] hoàn tất, đã xác nhận commit
    [partial] hoàn tất một phần, đã xác nhận commit
    [unavailable] không khả dụng, dự án không thay đổi
    [execution_failed] thực thi thất bại, chưa commit
    [commit_preparation_failed] chuẩn bị commit thất bại, chắc chắn chưa áp dụng
    [commit_not_applied] giao dịch chắc chắn chưa áp dụng
    [commit_outcome_unknown] không rõ kết quả commit
    [not_committed_after_earlier_failure] chưa commit do tác vụ trước thất bại
    [invalid_result] chuỗi kết quả Executor không hợp lệ, chưa commit
    [cancelled] đã hủy, chưa commit
   *[other] { $state }
}
task-record-requested = Bản dịch được yêu cầu: { $requested }
task-record-accepted-written = Đã nhận: { $accepted } mục (ID: { $ids }), ghi vào { $written } vị trí thực tế
task-record-accepted-outcome-unknown = Đã kiểm tra: { $accepted } mục (ID: { $ids }); không thể xác nhận kết quả commit cơ sở dữ liệu
task-record-unaccepted = Chưa được nhận: { $unaccepted } mục (ID: { $ids })
task-record-task-diagnostic = Chẩn đoán tác vụ
