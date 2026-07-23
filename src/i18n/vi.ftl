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
