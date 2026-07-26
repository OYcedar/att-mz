app-about = Dịch trò chơi RPG Maker với trạng thái dự án có thể tái sử dụng
cli-config-help = Tệp cấu hình TOML nghiêm ngặt cho lần chạy này
cli-ui-language-help = Ngôn ngữ cho trợ giúp, chẩn đoán, tiến độ, kết quả và nhật ký dự án: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko hoặc vi
cli-progress-help = Chế độ tiến độ trực tiếp: auto, plain hoặc off
cli-mz-about = Dịch trò chơi RPG Maker MZ
cli-mv-about = Dịch trò chơi RPG Maker MV
cli-init-about = Khởi tạo hoặc cập nhật dự án trò chơi có tên
cli-extract-about = Trích xuất văn bản bằng kế hoạch owner đã chỉ định hoặc đã lưu
cli-translate-about = Dịch văn bản đã trích xuất bằng Profile đã chỉ định hoặc đã lưu
cli-write-back-about = Ghi bản dịch đã duyệt trở lại trò chơi
cli-project-lua-about = Chạy một lần chương trình Lua tin cậy trong ngữ cảnh dự án
cli-project-name-help = Tên dự án ổn định
cli-init-path-help = Thư mục gốc trò chơi RPG Maker; dự án hiện có có thể dùng lại đường dẫn thành công gần nhất
cli-source-language-help = ID ngôn ngữ nguồn
cli-target-language-help = ID ngôn ngữ đích
cli-dialogue-width-help = Số ký tự toàn chiều rộng tối đa trên mỗi dòng hội thoại
cli-scrolling-width-help = Số ký tự toàn chiều rộng tối đa trên mỗi dòng văn bản cuộn
cli-help-width-help = Số ký tự toàn chiều rộng tối đa trên mỗi dòng trợ giúp hoặc mô tả
cli-builtin-help = Dùng các vị trí văn bản RPG Maker tích hợp của ATT
cli-rules-help = Thay owner Rules bằng định nghĩa TOML này; danh sách quy tắc rỗng sẽ tắt nó
cli-dialogue-rules-help = Thay phép chiếu tên hội thoại MV dùng cùng Builtin
cli-lua-help = Thay chương trình Lua của giai đoạn; tệp 0 byte sẽ xóa chương trình
cli-profile-help = ID Profile dịch; bỏ qua để dùng lại Profile thành công gần nhất
cli-terms-help = Thay tài nguyên thuật ngữ của dự án
cli-placeholders-help = Thay tài nguyên Placeholder của dự án
cli-project-lua-profile-help = Profile dùng để duyệt thủ công Standard; nếu bỏ qua, Profile Translate thành công gần nhất được dùng khi mở Standard
cli-project-lua-script-help = Chương trình Lua tin cậy sẽ chạy một lần
cli-project-lua-arguments-help = Đối số UTF-8 truyền cho Lua arg[1..] sau --
cli-usage-heading = Cách dùng:
cli-commands-heading = Lệnh:
cli-options-heading = Tùy chọn:
cli-arguments-heading = Đối số:
cli-options-metavar = TÙY_CHỌN
cli-command-metavar = LỆNH
cli-print-help = In trợ giúp
cli-print-version = In phiên bản
cli-missing-config = Thiếu đường dẫn cấu hình bắt buộc --config <FILE>.
cli-blank-value = Giá trị không được để trống.
cli-invalid-positive-integer = Giá trị phải là số nguyên dương.
cli-invalid-progress = Không hỗ trợ chế độ tiến độ { $value }; hãy dùng auto, plain hoặc off.
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
log-label-phase-check-project = kiểm tra dự án
log-label-phase-scan-source = quét nguồn
log-label-phase-prepare-candidate = chuẩn bị bản ứng viên
log-label-phase-update-database = cập nhật cơ sở dữ liệu
log-label-phase-publish = xuất bản
log-label-phase-builtin = trích xuất tích hợp
log-label-phase-rules = trích xuất theo quy tắc
log-label-phase-lua = xử lý Lua
log-label-phase-planning = lập kế hoạch
log-label-phase-confirmed-tasks = xác nhận tác vụ
log-label-phase-no-work = không cần xử lý
log-label-phase-read-assets = đọc tài nguyên
log-label-phase-plan-standard = lập kế hoạch ghi lại chuẩn
log-label-phase-rewrite-documents = ghi lại tài liệu
log-label-phase-validate-candidate = xác thực bản ứng viên
log-label-task-complete = hoàn tất
log-label-task-partial = một phần
log-label-task-unavailable = không khả dụng
log-label-task-failed = thất bại
error-state-applied-finalization = Kết quả đã có hiệu lực nhưng bước hoàn tất thất bại. Hãy kiểm tra trạng thái dự án trước khi thử lại.
error-no-executable-extract-owner = Sau khi xóa không còn owner Extract có thể chạy, vì vậy kế hoạch không được lưu.
error-plan-save-failed-applied = Kết quả lệnh đã có hiệu lực nhưng kế hoạch chạy mới không được lưu. Lần tới hãy chỉ định rõ các tùy chọn mong muốn.
error-plan-save-outcome-unknown = Kết quả lệnh đã có hiệu lực nhưng không thể xác nhận commit kế hoạch chạy. Lần tới hãy chỉ định rõ các tùy chọn mong muốn.
plan-source-explicit = đầu vào chỉ định
plan-source-project-state = trạng thái dự án
plan-source-product-default = hành vi sản phẩm
notice-init-reuse-path = Không có đường dẫn nguồn; đang dùng lại đường dẫn thành công gần nhất: { $path }.
notice-extract-reuse-owners = Không có phạm vi trích xuất; đang dùng lại kế hoạch thành công gần nhất: { $owners }.
notice-translate-reuse-profile = Không có Profile; đang dùng lại Profile thành công gần nhất: { $profile }.
notice-translate-reuse-lua = Không có tùy chọn Lua; đang dùng lại lựa chọn Translate Lua thành công gần nhất.
notice-write-back-reuse-lua = Không có tùy chọn Lua; đang dùng lại chương trình WriteBack Lua thành công gần nhất.
notice-write-back-standard-only = Chưa cấu hình chương trình WriteBack Lua; chỉ chạy Standard.
notice-owner-disabled = Owner { $owner } đã bị tắt và xóa khỏi các kế hoạch tự động sau này.
notice-lua-cleared = Chương trình Lua { $phase } đã bị xóa và sẽ không chạy lần này.
notice-no-model-request = Mọi đơn vị dịch chuẩn đều mới nhất; trong lần chạy này Standard không gửi yêu cầu nào đến mô hình.
notice-manual-layout = Có { $count } đơn vị cần kiểm tra ngắt dòng thủ công.
notice-log-degraded = Nhật ký dự án không khả dụng hoặc suy giảm; lệnh vẫn tiếp tục và trạng thái thoát không đổi.
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
progress-extract-lua = Đang chạy chương trình Extract Lua
progress-extract-commit = Đang commit tài sản đã trích xuất
progress-translate-planning = Đang lập kế hoạch tác vụ dịch
progress-translate-confirmed = Tác vụ dịch đã xác nhận
progress-translate-no-work = Không cần gọi mô hình
progress-project-lua = Đang chạy chương trình Lua của dự án
progress-write-back-read-assets = Đang đọc tài sản đã duyệt
progress-write-back-planning = Đang lập kế hoạch viết lại tài liệu
progress-write-back-documents = Đã viết lại tài liệu
progress-write-back-lua = Đang chạy chương trình WriteBack Lua
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
result-translate-completed = Dịch hoàn tất: { $project } (Profile: { $profile })
result-translate-standard = Dịch chuẩn: { $total } tác vụ; { $complete } hoàn tất, { $partial } một phần, { $unavailable } không khả dụng; đã ghi { $written } vị trí, còn { $remaining }
result-translate-convergence = Hội tụ trạng thái: giữ { $retained }, vô hiệu { $invalidated }, không áp dụng { $not_applicable }, tái dùng { $reused }
result-write-back-completed = Ghi lại hoàn tất: { $project }
result-project-lua-completed = Thực thi Lua dự án hoàn tất: { $project }
result-output-directory = Thư mục đầu ra: { $path }
result-write-back-standard = Ghi lại chuẩn: { $translated } đơn vị dịch, { $original } đơn vị nguồn; tự ngắt { $auto_wrapped }, thêm { $breaks } ngắt dòng và { $indents } thụt đầu dòng toàn chiều rộng; { $manual } cần bố cục thủ công
result-lua-executed = Lua: đã chạy
result-lua-not-executed = Lua: không chạy
result-cancelled = Lệnh đã bị hủy sau khi hoàn tất an toàn.
result-plan-saved = Kế hoạch chạy thành công đã được lưu.
result-translate-plan-sources = Đã lưu kế hoạch của lần chạy thành công này. Nguồn Profile: { $profile_source }; nguồn Lua: { $lua_source }.
log-run-started = Lệnh { $command } đã bắt đầu.
log-run-succeeded = Lệnh { $command } đã hoàn tất thành công.
log-run-failed = Lệnh { $command } thất bại.
log-run-outcome-unknown = Lệnh { $command } đã kết thúc nhưng kết quả cuối cùng chưa xác định; hãy làm theo các vị trí khôi phục trong lỗi.
log-run-cancelled = Lệnh { $command } đã bị hủy.
log-performance-counters = Bộ đếm hiệu năng: số lần thử điều khiển giao dịch SQLite { $sqlite_control_attempted_total }; xác thực toàn bộ cây ứng viên đã bắt đầu { $candidate_validation_started }, đã hoàn tất { $candidate_validation_completed }.
log-plan-resolved = Lệnh { $command } lấy kế hoạch từ { $source }.
log-phase-started = Bắt đầu giai đoạn: { $phase }.
log-phase-finished = Hoàn tất giai đoạn: { $phase }.
log-retry-summary = Đã thực hiện { $count } lần thử lại.
log-no-work = Không cần công việc: { $reason }.
log-no-work-translation-up-to-date = bản dịch đã khớp với nguồn và hồ sơ hiện tại
log-partial-result = Có { $count } kết quả một phần cần chú ý.
log-translation-task-started = Tác vụ dịch { $index }/{ $total } đã bắt đầu.
log-translation-task-finished = Tác vụ dịch { $index } kết thúc với kết quả { $outcome }.
log-translation-task-diagnostic = Tác vụ dịch { $index } báo chẩn đoán sau { $attempts } lần thử: { $diagnostic }
diagnostic-title = Lỗi [{ $code }]
diagnostic-stage = Giai đoạn: { $stage }
diagnostic-subject = Vị trí: { $subject }
diagnostic-subject-value = { $kind ->
    [command] lệnh { $value }
    [field] trường { $value }
    [project] dự án { $value }
    [profile] hồ sơ { $value }
    [component] thành phần { $value }
   *[other] { $value }
}
diagnostic-reason = Nguyên nhân: { $reason }
diagnostic-impact = Ảnh hưởng: { $impact }
diagnostic-action = Cách xử lý: { $action }
diagnostic-recovery = Khôi phục: { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] thành phần { $value }
    [transaction] giao dịch { $value }
   *[other] { $value }
}
diagnostic-related = Lỗi liên quan { $index }:
diagnostic-stage-value = { $code ->
    [process_output] Đầu ra tiến trình
    [lua] Thực thi Lua dự án
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
task-record-title = Tác vụ dịch { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] Hoàn tất
    [partial] Hoàn tất một phần
    [unavailable] Không khả dụng
    [execution_failed] Thực thi thất bại
    [commit_preparation_failed] Chuẩn bị commit thất bại
    [commit_not_applied] Commit chưa được áp dụng
    [commit_outcome_unknown] Không rõ kết quả commit
    [not_committed_after_earlier_failure] Chưa commit do lỗi trước đó
    [invalid_result] Chuỗi kết quả Executor không hợp lệ
    [cancelled] Đã hủy
   *[other] { $state }
}
task-record-summary-with-written = `Tác vụ { $ordinal }/{ $total }` · `{ $attempts } lần thử` · `Đã nhận { $accepted }/{ $expected }` · `Ghi vào { $written } vị trí`
task-record-summary-without-written = `Tác vụ { $ordinal }/{ $total }` · `{ $attempts } lần thử` · `Đã nhận { $accepted }/{ $expected }`
task-record-run-id-label = ID lượt chạy:
task-record-started-at-label = Bắt đầu:
task-record-duration-label = Tổng thời gian:
task-record-endpoint-label = Endpoint:
task-record-model-label = Mô hình:
task-record-custom-parameters-heading = Tham số tùy chỉnh
task-record-attempts-heading = Các lần gửi yêu cầu
task-record-final-result-heading = Kết quả cuối
task-record-no-request = Không tạo được yêu cầu mô hình sẵn sàng để gửi.
task-record-empty-assistant = Mô hình trả về một đối tượng rỗng.
task-record-parse-error = Lỗi phân tích: { $kind ->
    [json] JSON phản hồi của mô hình không hợp lệ (loại `{ $category }`), dòng { $line }, cột { $column }
    [thinking_not_allowed] chế độ phản hồi hiện tại không chấp nhận phần suy luận, dòng { $line }, cột { $column }
    [thinking_envelope_missing] thiếu phong bì suy luận bắt buộc, dòng { $line }, cột { $column }
    [thinking_envelope_unclosed] phong bì suy luận chưa được đóng, dòng { $line }, cột { $column }
    [thinking_empty] nội dung suy luận trống, dòng { $line }, cột { $column }
    [thinking_nested] có phong bì suy luận lồng nhau, dòng { $line }, cột { $column }
    [thinking_repeated] có phong bì suy luận lặp lại, dòng { $line }, cột { $column }
    [markdown_fence_no_body] hàng rào Markdown không có nội dung, dòng { $line }, cột { $column }
    [markdown_fence_unsupported] chỉ chấp nhận một hàng rào Markdown không có nhãn ngôn ngữ hoặc có nhãn json, dòng { $line }, cột { $column }
    [markdown_fence_unclosed] hàng rào Markdown chưa được đóng, dòng { $line }, cột { $column }
   *[markdown_fence_invalid_closing] hàng rào Markdown phải đóng ở dòng độc lập cuối cùng, dòng { $line }, cột { $column }
}
task-record-attempt-succeeded = Lần thử { $number }: thành công; finish reason { $finish_reason }
task-record-attempt-token-usage = ; token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; thời gian `{ $duration }`
task-record-attempt-request-id = ; request ID { $request_id }
task-record-attempt-response-id = ; response ID { $response_id }
task-record-attempt-retryable = Lần thử { $number }: yêu cầu lỗi có thể thử lại; chẩn đoán `{ $code }`; thời gian `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; thử lại sau `{ $duration }`
task-record-attempt-wait-completed = ; đã chờ xong `{ $duration }`; lần thử tiếp theo chưa bắt đầu
task-record-attempt-wait-cancelled = ; dự kiến chờ `{ $duration }`; đã hủy trong lúc chờ
task-record-attempt-failed = Lần thử { $number }: xử lý yêu cầu hoặc phản hồi thất bại; chẩn đoán `{ $code }`; thời gian `{ $duration }`
task-record-attempt-cancelled = Lần thử { $number }: đã hủy; thời gian `{ $duration }`
task-record-structured-reason = Lý do: { $reason }
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
task-record-accepted-written = Đã nhận: { $accepted } mục, ghi vào { $written } vị trí thực tế
task-record-accepted-outcome-unknown = Đã kiểm tra: { $accepted } mục; không thể xác nhận kết quả commit cơ sở dữ liệu
task-record-rejected-heading = Không được nhận:
task-record-rejected-item = { $id }: { $reason }
task-record-protocol-diagnostic = Chẩn đoán giao thức: { $diagnostic }
task-record-unavailable-reason = Lý do không khả dụng: { $reason }
task-record-task-diagnostic = Chẩn đoán tác vụ: `{ $code }`; lý do { $reason }
task-record-rejection-reason = { $code ->
    [missing] Thiếu đầu ra mô hình
    [duplicate] Đầu ra mô hình bị lặp
    [invalid_shape] { $detail }
    [invalid_shape_array] Bản dịch phải là một mảng chuỗi
    [invalid_shape_item] Mục { $line } của mảng bản dịch phải là chuỗi
    [line_count_mismatch] Số dòng không khớp (mong đợi { $expected }, thực tế { $actual })
    [invalid_line_text] Dòng { $line } chứa ký tự điều khiển không hợp lệ
    [blank_line_mismatch] Trạng thái trống ở dòng { $line } không khớp (mong đợi: { $expected_blank ->
        [blank] trống
       *[other] không trống
    })
    [blank_translation] Bản dịch trống
    [no_natural_language_text] Bản dịch không có văn bản ngôn ngữ tự nhiên
    [contains_byte_order_mark] Bản dịch chứa BOM
    [placeholder_mismatch] Placeholder không khớp: { $detail }
    [unexpected_placeholder] Placeholder không mong đợi: { $detail }
    [placeholder_normalization_ambiguous] Chuẩn hóa placeholder không rõ ràng: { $detail }
    [source_residual] Phát hiện phần còn lại của ngôn ngữ nguồn: { $detail }
    [tag_value_contains_closing_delimiter] Dòng { $line } chứa '>' sẽ đóng giá trị thẻ sớm
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason không phải stop: { $detail }
    [invalid_response] { $detail }
    [invalid_id] Mục mô hình thứ { $index } có ID không hợp lệ
    [unknown_id] Mục mô hình thứ { $index } trả về ID lạ { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] Không thể phân tích phản hồi mô hình
    [all_outputs_rejected] Mọi đầu ra mô hình đều không vượt qua kiểm tra
    [recoverable_request_exhausted] Đã hết ngân sách thử lại cho yêu cầu có thể phục hồi
    [retry_after_exceeds_maximum] Retry-After vượt quá thời gian chờ tối đa đã cấu hình
   *[other] { $code }
}
task-record-duration-seconds = { $value } giây
task-record-duration-milliseconds = { $value } mili giây
