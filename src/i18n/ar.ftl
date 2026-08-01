app-about = ترجمة الألعاب والنصوص المنظّمة باستخدام حالة مشروع قابلة لإعادة الاستخدام
cli-ui-language-help = لغة المساعدة والتشخيص والتقدم والنتائج وسجلات المشروع: ar وzh-Hans وzh-Hant وen وfr وru وes وja وko وvi
cli-progress-help = نمط التقدم المباشر: auto أو plain أو off
cli-mz-about = ترجمة لعبة RPG Maker MZ
cli-mv-about = ترجمة لعبة RPG Maker MV
cli-generic-about = ترجمة نص JSONL منظّم
cli-init-about = تهيئة مشروع ترجمة مسمى أو تحديثه
cli-extract-about = مزامنة النص المصدر من مدخل المشروع الحالي
cli-translate-about = ترجمة النص المستخرج باستخدام Profile صريح أو محفوظ
cli-write-back-about = كتابة الترجمات الحالية إلى مخرجات المشروع
cli-project-lua-about = تشغيل Lua ذري لقاعدة البيانات مرة واحدة داخل المشروع
cli-project-name-help = اسم المشروع الثابت
cli-init-path-help = دليل جذر الإدخال؛ يمكن للمشروع الموجود إعادة استخدام آخر مسار ناجح
cli-source-language-help = معرّف لغة المصدر
cli-target-language-help = معرّف اللغة الهدف
cli-dialogue-width-help = الحد الأقصى للمحارف كاملة العرض في سطر الحوار
cli-scrolling-width-help = الحد الأقصى للمحارف كاملة العرض في سطر النص المتمرر
cli-help-width-help = الحد الأقصى للمحارف كاملة العرض في سطر المساعدة أو الوصف
cli-builtin-help = استخدام مواضع نص RPG Maker المضمنة في ATT
cli-rules-help = استبدال قواعد استخراج RPG Maker بتعريف TOML هذا؛ القائمة الفارغة تعطّل القواعد
cli-dialogue-rules-help = استبدال إسقاط أسماء حوار MV المستخدم مع Builtin
cli-profile-help = معرّف Profile للترجمة؛ يؤدي حذفه إلى إعادة استخدام آخر Profile ناجح
cli-terms-help = استبدال مورد مصطلحات المشروع
cli-placeholders-help = استبدال مورد Placeholder للمشروع
cli-project-lua-script-help = برنامج Lua ذري لقاعدة البيانات يُشغّل مرة واحدة
cli-project-lua-arguments-help = وسيطة UTF-8 تمرر إلى Lua arg[1..] بعد --
cli-usage-heading = الاستخدام:
cli-commands-heading = الأوامر:
cli-options-heading = الخيارات:
cli-arguments-heading = الوسائط:
cli-options-metavar = خيارات
cli-command-metavar = أمر
cli-print-help = عرض المساعدة
cli-print-version = عرض الإصدار
cli-blank-value = لا يجوز أن تكون القيمة فارغة.
cli-invalid-positive-integer = يجب أن تكون القيمة عددًا صحيحًا موجبًا.
cli-invalid-progress = نمط التقدم { $value } غير مدعوم؛ استخدم auto أو plain أو off.
cli-invalid-ui-language-argument = يحتوي --ui-language على وسم لغة غير صالح: { $value }.
cli-unsupported-ui-language-argument = يطلب --ui-language لغة غير مدعومة: { $value }.
cli-invalid-ui-language-environment = يحتوي ATT_UI_LANGUAGE على وسم لغة غير صالح: { $value }.
cli-unsupported-ui-language-environment = يطلب ATT_UI_LANGUAGE لغة غير مدعومة: { $value }.
cli-ui-language-environment-not-unicode = قيمة ATT_UI_LANGUAGE ليست Unicode صالحًا.
cli-unexpected-argument = وسيط غير متوقع: { $value }.
cli-missing-required-argument = وسيط مطلوب مفقود: { $value }.
cli-invalid-value = القيمة { $value } غير صالحة لـ { $argument }.
cli-error-heading = خطأ:
cli-try-help = لمزيد من المعلومات، استخدم --help.
cli-missing-value = يجب توفير قيمة لـ { $argument }.
cli-missing-subcommand = يجب توفير أمر.
cli-argument-conflict = لا يمكن استخدام { $argument } مع الوسائط الأخرى المقدمة.
cli-wrong-number-of-values = تم توفير عدد غير صحيح من القيم لـ { $argument }.
cli-invalid-utf8 = أحد وسائط سطر الأوامر ليس Unicode صالحًا.
cli-parse-failure = تعذر تحليل سطر الأوامر.
log-label-phase-check-project = فحص المشروع
log-label-phase-scan-source = فحص المصدر
log-label-phase-prepare-candidate = إعداد المرشح
log-label-phase-update-database = تحديث قاعدة البيانات
log-label-phase-publish = النشر
log-label-phase-builtin = الاستخراج المدمج
log-label-phase-rules = الاستخراج بالقواعد
log-label-phase-lua = معالجة Lua
log-label-phase-planning = التخطيط
log-label-phase-confirmed-tasks = تأكيد المهام
log-label-phase-no-work = لا عمل مطلوب
log-label-phase-read-assets = قراءة الأصول
log-label-phase-plan-rpg-maker-write-back = تخطيط كتابة RPG Maker
log-label-phase-rewrite-documents = إعادة كتابة المستندات
log-label-phase-validate-candidate = التحقق من المرشح
log-label-task-complete = مكتمل
log-label-task-partial = جزئي
log-label-task-unavailable = غير متاح
log-label-task-failed = فشل
error-state-applied-finalization = طُبقت النتيجة لكن الإنهاء فشل. افحص حالة المشروع قبل إعادة المحاولة.
error-no-executable-extract-owner = لم يبقَ بعد المسح أي owner Extract قابل للتنفيذ، لذلك لم تُحفظ الخطة.
error-plan-save-failed-applied = طُبقت نتيجة الأمر لكن خطة التشغيل الجديدة لم تُحفظ. مرّر الخيارات المطلوبة صراحةً في المرة القادمة.
error-plan-save-outcome-unknown = طُبقت نتيجة الأمر لكن تعذر تأكيد commit خطة التشغيل. مرّر الخيارات المطلوبة صراحةً في المرة القادمة.
plan-source-explicit = إدخال صريح
plan-source-project-state = حالة المشروع
plan-source-product-default = سلوك المنتج
notice-init-reuse-path = لم يُقدّم مسار مصدر؛ سيُعاد استخدام آخر مسار ناجح: { $path }.
notice-extract-reuse-owners = لم يُقدّم نطاق استخراج؛ ستُعاد استخدام آخر خطة ناجحة: { $owners }.
notice-translate-reuse-profile = لم يُقدّم Profile؛ سيُعاد استخدام آخر Profile ناجح: { $profile }.
notice-owner-disabled = عُطّل owner { $owner } وأزيل من الخطط التلقائية اللاحقة.
warning-rules-command-non-string-skipped = تحذير: تخطّت قاعدة Rules رقم { $rule_number } عدد { $skipped_count } من معاملات command غير النصية (المصدر { $source_file }، code={ $command_code }، parameter={ $parameter }، النوع { $actual_type }).
warning-manual-layout-required = تحذير: يلزم فحص فواصل الأسطر يدويًا في { $locations } (region={ $region }، max_fullwidth_chars={ $max_fullwidth_chars }).
notice-no-model-request = كل وحدات الترجمة حديثة؛ لم تحتج هذه الجولة إلى إرسال طلب للنموذج.
notice-manual-layout = { $count ->
    [zero] لا توجد وحدات تحتاج إلى مراجعة يدوية لفواصل الأسطر.
    [one] تحتاج وحدة واحدة إلى مراجعة يدوية لفواصل الأسطر.
    [two] تحتاج وحدتان إلى مراجعة يدوية لفواصل الأسطر.
    [few] تحتاج { $count } وحدات إلى مراجعة يدوية لفواصل الأسطر.
    [many] تحتاج { $count } وحدة إلى مراجعة يدوية لفواصل الأسطر.
   *[other] تحتاج { $count } وحدة إلى مراجعة يدوية لفواصل الأسطر.
}
notice-log-degraded = سجل المشروع غير متاح أو متدهور؛ سيستمر الأمر ولن تتغير حالة الخروج.
notice-task-records-degraded = سجلات مهام الترجمة غير متاحة أو متدهورة؛ سيستمر الأمر ولن تتغير حالة الخروج.
progress-init-check-project = جارٍ فحص حالة المشروع
progress-init-scan-source = جارٍ فحص مصدر اللعبة
progress-init-build-candidate = جارٍ بناء مرشح المشروع
progress-init-converge-database = جارٍ تقارب قاعدة بيانات المشروع
progress-init-publish = جارٍ نشر المشروع المهيأ
progress-save-run-plan = جارٍ حفظ خطة التشغيل الناجحة
progress-extract-owner = owner الاستخراج: { $owner }
progress-extract-documents = جارٍ فحص المستندات
progress-extract-builtin = وحدات عمل Builtin
progress-extract-rules = تعريفات Rules
progress-extract-commit = جارٍ تنفيذ commit للأصول المستخرجة
progress-generic-init = جارٍ تهيئة مشروع Generic
progress-generic-extract = جارٍ فحص إدخال Generic JSONL
progress-translate-planning = جارٍ تخطيط مهام الترجمة
progress-translate-confirmed = مهام الترجمة المؤكدة
progress-translate-no-work = لا حاجة إلى طلب النموذج
progress-project-lua = جارٍ تشغيل برنامج Lua للمشروع
progress-write-back-read-assets = جارٍ قراءة الأصول المقبولة
progress-write-back-planning = جارٍ تخطيط إعادة كتابة المستندات
progress-write-back-documents = المستندات المعاد كتابتها
progress-write-back-validate-candidate = جارٍ التحقق من مرشح الإخراج
progress-write-back-publish = جارٍ نشر الإخراج؛ سينتظر الانقطاع نتيجة مؤكدة
progress-finalizing = جارٍ إنهاء الموارد المطلوبة
progress-safe-stopping = جارٍ التوقف بأمان مع الاحتفاظ بآخر تقدم مؤكد
result-init-completed = اكتملت التهيئة: { $project }
result-init-created = حالة المشروع: أُنشئ
result-init-unchanged = حالة المشروع: بلا تغيير
result-init-updated = حالة المشروع: حُدّث
result-init-stale-owners = يلزم إعادة الاستخراج: { $owners }
result-extract-completed = اكتمل الاستخراج: { $project }
result-translate-completed = اكتملت الترجمة: { $project } (Profile: { $profile })
result-translate-summary = الترجمة: { $total } مهمة؛ مكتملة { $complete }، جزئية { $partial }، غير متاحة { $unavailable }؛ كُتب { $written } موضعًا وتبقى { $remaining }
result-translate-convergence = تقارب الحالة: أُبقي { $retained }، أُبطل { $invalidated }، غير منطبق { $not_applicable }، أُعيد استخدام { $reused }
result-write-back-completed = اكتملت الكتابة: { $project }
result-project-lua-completed = اكتمل تنفيذ Lua للمشروع: { $project }
result-output-directory = مجلد الإخراج: { $path }
result-write-back-summary = الكتابة: { $translated } وحدة مترجمة و{ $original } وحدة مصدر؛ التفاف تلقائي { $auto_wrapped }، أضيف { $breaks } فاصل أسطر و{ $indents } إزاحة كاملة العرض؛ يحتاج { $manual } إلى تخطيط يدوي
result-generic-extract-unchanged = لم تتغير مدخلات Generic: ‏{ $files } ملفًا و{ $groups } مجموعة و{ $units } وحدة
result-generic-extract-updated = حُدثت مدخلات Generic: ‏{ $files } ملفًا و{ $groups } مجموعة و{ $units } وحدة؛ حُفظت { $preserved } ترجمة ومُسحت { $cleared }
result-generic-translate-summary = ترجمة Generic: ‏{ $total } مهمة؛ مكتملة { $complete }، جزئية { $partial }، غير متاحة { $unavailable }؛ مُسحت { $cleared }، وأُعيد استخدام { $reused }، وقُبل { $accepted }، وكُتب { $written }، والتعارضات { $conflicted }، ومشكلات الاستجابة { $problems }
result-generic-write-back-summary = كتابة Generic: ‏{ $translated } وحدة مترجمة مع الاحتفاظ بـ { $original } وحدة مصدر
result-cancelled = أُلغي الأمر بعد إنهاء آمن.
result-plan-saved = حُفظت خطة التشغيل الناجحة.
log-run-started = بدأ الأمر { $command }.
log-run-succeeded = اكتمل الأمر { $command } بنجاح.
log-run-failed = فشل الأمر { $command }.
log-run-outcome-unknown = انتهى الأمر { $command } لكن النتيجة النهائية غير معروفة؛ اتبع مواقع الاسترداد الواردة في الخطأ.
log-run-cancelled = أُلغي الأمر { $command }.
log-performance-counters = عدادات الأداء: محاولات التحكم في معاملات SQLite‏ { $sqlite_control_attempted_total }؛ بدء التحقق الكامل من شجرة المرشح { $candidate_validation_started }، واكتماله { $candidate_validation_completed }.
log-lua-script = برنامج Lua النصي { $identity } ‏(SHA-256 { $fingerprint }).
log-lua-print = Lua: { $message }
log-lua-summary = تم تثبيت Lua: استدعاءات قاعدة البيانات { $database_calls }، والصفوف المعدلة { $changed_rows }، واستدعاءات الترجمة { $translation_calls }، وأسطر print‏ { $printed_lines }.
log-plan-resolved = حُلّت خطة الأمر { $command } من { $source }.
log-phase-started = بدأت المرحلة: { $phase }.
log-phase-finished = اكتملت المرحلة: { $phase }.
log-retry-summary = { $count ->
    [zero] لا توجد محاولات إعادة.
    [one] نُفذت محاولة واحدة.
    [two] نُفذت محاولتان.
    [few] نُفذت { $count } محاولات.
    [many] نُفذت { $count } محاولة.
   *[other] نُفذت { $count } محاولة.
}
log-no-work = لم يلزم أي عمل: { $reason }.
log-no-work-translation-up-to-date = الترجمات مطابقة بالفعل للمصدر والملف الشخصي الحاليين
log-partial-result = { $count ->
    [zero] لا توجد نتائج جزئية تحتاج إلى انتباه.
    [one] توجد نتيجة جزئية واحدة تحتاج إلى انتباه.
    [two] توجد نتيجتان جزئيتان تحتاجان إلى انتباه.
    [few] توجد { $count } نتائج جزئية تحتاج إلى انتباه.
    [many] توجد { $count } نتيجة جزئية تحتاج إلى انتباه.
   *[other] توجد { $count } نتيجة جزئية تحتاج إلى انتباه.
}
log-translation-task-started = بدأت مهمة الترجمة { $index }/{ $total }.
log-translation-task-finished = انتهت مهمة الترجمة { $index } بالنتيجة { $outcome }.
log-translation-task-diagnostic = أبلغت مهمة الترجمة { $index } عن تشخيص بعد { $attempts } محاولات: { $diagnostic }
diagnostic-title = خطأ [{ $code }]
diagnostic-stage = المرحلة: { $stage }
diagnostic-subject = الموقع: { $subject }
diagnostic-subject-value = { $kind ->
    [command] الأمر { $value }
    [field] الحقل { $value }
    [project] المشروع { $value }
    [profile] الملف الشخصي { $value }
    [component] المكوّن { $value }
   *[other] { $value }
}
diagnostic-reason = السبب: { $reason }
diagnostic-impact = التأثير: { $impact }
diagnostic-action = الإجراء: { $action }
diagnostic-recovery = الاسترداد: { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] المكوّن { $value }
    [transaction] المعاملة { $value }
   *[other] { $value }
}
diagnostic-related = الخطأ المرتبط { $index }:
diagnostic-stage-value = { $code ->
    [process_startup] بدء العملية
    [process_output] إخراج العملية
    [configuration] تحميل الإعدادات
    [command_preparation] إعداد الأمر
    [project_opening] فتح المشروع
    [init] التهيئة
    [extract] الاستخراج
    [translate] الترجمة
    [write_back] إعادة الكتابة
    [lua] تنفيذ Lua للمشروع
    [model_request] طلب النموذج
    [run_plan_finalization] إنهاء خطة التشغيل
    [publication] النشر
    [shutdown] الإغلاق
    [logging] سجل المشروع
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] لم تتغير الحالة
    [valid_progress_preserved] حُفظ التقدم الصالح
    [result_applied_but_run_plan_not_saved] طُبقت النتيجة، لكن لم تُحفظ خطة التشغيل
    [state_applied_but_finalization_failed] طُبقت الحالة، لكن لم يكتمل الإنهاء
    [recovery_required] يلزم الاسترداد قبل الوثوق بالحالة
    [outcome_unknown] الحالة النهائية غير معروفة
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] صحح حقل الإعدادات المحدد ثم أعد المحاولة
    [fix_input] صحح الإدخال المحدد ثم أعد المحاولة
    [check_path_and_permissions] تحقق من المسار وحالة نظام الملفات والأذونات
    [check_project_state] افحص حالة المشروع وصححها ثم أعد المحاولة
    [retry_after_resolving_contention] انتظر انتهاء العملية المتعارضة ثم أعد المحاولة
    [check_model_service] تحقق من استجابة خدمة النموذج وحدود الحساب
    [preserve_recovery_artifacts] لا تحذف عناصر الاسترداد المدرجة؛ استرد المخرجات قبل إعادة المحاولة
    [retry] أعد محاولة العملية
    [report_bug] أبلغ عن عيب ATT هذا مع رمز الخطأ ومسار السجل
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] قيمة مطلوبة مفقودة
    [extract_plan_required] لا توجد خطة Extract محفوظة قابلة لإعادة الاستخدام؛ حدد --builtin أو --rules
    [generic_extract_required] لم يعد إدخال JSONL مطابقًا لآخر Extract؛ شغّل att generic extract مرة أخرى
    [conflicting_values] القيم المقدمة متعارضة
    [invalid_syntax] صياغة القيمة غير صالحة
    [invalid_encoding] ترميز النص غير صالح
    [invalid_value] القيمة تخالف العقد المطلوب
    [not_found] الكائن المطلوب غير موجود
    [busy] المورد مستخدم بواسطة عملية أخرى
    [state_mismatch] حالة المشروع المحفوظة لا تستوفي متطلبات هذه العملية
    [requirement_failed] شرط مسبق مطلوب غير مستوفى
    [transaction_rolled_back] فشلت المعاملة وتراجعت تغييراتها
    [transaction_outcome_unknown] انتهت المعاملة دون تأكيد التثبيت أو التراجع
    [finalization_failed] نتيجة العملية موجودة لكن الإنهاء فشل
    [rollback_failed] فشلت العملية الأساسية وفشل التراجع أيضًا
    [external_service_rejected] رفضت الخدمة الخارجية الطلب
    [external_service_unavailable] الخدمة الخارجية غير متاحة
    [executor_closed] خدمة التنفيذ قيد الإغلاق أو مغلقة بالفعل
    [concurrent_shutdown] مستدعٍ آخر يغلق المنفّذ بالفعل
    [executor_state_poisoned] حالة دورة حياة المنفّذ تالفة
    [worker_spawn_failed] تعذر على نظام التشغيل إنشاء خيط العامل
    [worker_channel_closed] أُغلقت قناة أوامر العامل قبل اكتمال الإنهاء
    [worker_panicked] انتهى عامل على نحو غير متوقع
    [reparse_point_forbidden] يحتوي المسار على نقطة إعادة تحليل لا يمكن الوثوق بها
    [non_local_volume] المسار ليس على وحدة تخزين محلية ثابتة
    [non_ntfs_volume] المسار ليس على وحدة تخزين NTFS
    [case_sensitive_directory] يستخدم الدليل دلالات أسماء حساسة لحالة الأحرف
    [lock_cancelled] أُلغي انتظار القفل المطلوب
    [target_already_exists] الوجهة موجودة بالفعل
    [file_identity_changed] تغيرت هوية الملف أثناء العملية
    [invalid_path] المسار ليس هدفًا صالحًا لهذه العملية
    [wrong_publisher_instance] رمز النشر يخص مثيل ناشر آخر
    [journal_corrupt] سجل استرداد النشر غير صالح أو غير مكتمل
    [unexpected_artifact] عنصر غير متوقع في نظام الملفات يمنع العملية
    [interactive_session_already_open] جلسة SQLite تفاعلية أخرى نشطة بالفعل
    [backup_incomplete] لم تصل نسخة SQLite الاحتياطية إلى حالة الاكتمال
    [request_serialization_failed] تعذر إجراء تسلسل لطلب النموذج
    [response_parsing_failed] استجابة النموذج ليست JSON صالحًا
    [invalid_response_contract] استجابة النموذج لا تستوفي عقد الاستجابة المطلوب
    [transport_failed] فشل نقل HTTP قبل وصول استجابة صالحة
    [lua_database_open_failed] تعذر على مضيف Lua فتح جلسة قاعدة بيانات المشروع
    [lua_context_creation_failed] تعذر على وقت تشغيل Lua إنشاء سياق VM
    [lua_compilation_failed] تعذر تجميع برنامج Lua الرئيسي
    [lua_execution_failed] فشل برنامج Lua الرئيسي أثناء التشغيل
    [lua_host_call_failed] فشل استدعاء إحدى إمكانات مضيف Lua
    [lua_finalization_failed] تعذر على مضيف Lua إنهاء جميع الموارد المرتبطة
    [rules_definition_invalid] برنامج Rules لا يستوفي عقد تعريف Rules
    [rules_document_read_failed] تعذرت قراءة مستند مصدر يتطلبه برنامج Rules
    [rules_no_non_blank_match] لم ينتج إدخال Rules وحدة دلالية غير فارغة
    [rules_invalid_target] اختار إدخال Rules قيمة لا تصلح كهدف نصي
    [rules_pattern_match_failed] تعذر تقييم نمط PCRE2 في Rules
    [rules_zero_width_match] أنتج نمط Rules تطابقًا بعرض صفري
    [rules_overlapping_capture] أنتج نمط Rules لقطات نصية متداخلة
    [rules_missing_text_capture] لم تشارك لقطة النص المسماة المطلوبة في التطابق
    [rules_invalid_capture_range] تطابق Rules أو نطاق اللقطة خارج حدود أحرف UTF-8 الصالحة
    [rules_duplicate_target] يطالب إدخالان في Rules بنفس هدف النص الفعلي
    [rules_invalid_materialization] لا تستطيع وصفة إسقاط Rules إعادة بناء قيمة المصدر
    [rules_snapshot_invalid] مجموعات Rules المستخرجة لا تكوّن لقطة أصول صالحة
    [rules_snapshot_store_failed] تعذر تثبيت لقطة استخراج Rules التي تم التحقق منها
    [write_back_extraction_out_of_date] لم تعد الأصول المستخرجة تطابق مصدر المشروع الحالي
    [write_back_asset_snapshot_invalid] أصول RPG Maker المخزنة لا تكوّن لقطة إعادة كتابة صالحة
    [source_document_invalid] مستند مصدر RPG Maker لا يستوفي تنسيق المستند المطلوب
    [generic_source_document_invalid] مستند مصدر Generic JSONL لا يستوفي التنسيق المطلوب
    [write_back_mutation_invalid] لا يمكن تطبيق تعديل ترجمة متحقق منه على موضع المصدر المجمّد
    [write_back_output_path_invalid] الملف المعاد كتابته خارج شجرة إخراج RPG Maker المسموح بها
    [write_back_output_path_duplicate] أكثر من ملف معاد كتابته يستهدف مسار الإخراج نفسه
    [write_back_candidate_project_mismatch] مرشح إعادة الكتابة المحضر يخص مشروعًا آخر
    [write_back_candidate_invalid] مرشح إعادة الكتابة لا يستوفي بنية شجرة data/js المطلوبة
    [write_back_not_published] لم يستبدل مرشح إعادة الكتابة دليل الإخراج الحالي
    [write_back_published_with_residuals] نُشر الإخراج، لكن تعذرت إزالة عنصر استرداد واحد أو أكثر
    [write_back_recovery_required] يلزم استرداد دليل الإخراج قبل الوثوق بمحتوياته
    [internal_invariant] انتُهك ثابت داخلي؛ هذا عيب في ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] غير موجود
    [permission_denied] الإذن مرفوض
    [connection_refused] الاتصال مرفوض
    [connection_reset] أُعيد تعيين الاتصال
    [host_unreachable] لا يمكن الوصول إلى المضيف
    [network_unreachable] لا يمكن الوصول إلى الشبكة
    [connection_aborted] أُحبط الاتصال
    [not_connected] غير متصل
    [address_in_use] العنوان مستخدم بالفعل
    [address_not_available] العنوان غير متاح
    [network_down] الشبكة متوقفة
    [broken_pipe] الأنبوب مقطوع
    [already_exists] موجود بالفعل
    [would_block] ستؤدي العملية إلى الحظر
    [not_a_directory] ليس دليلاً
    [is_a_directory] هو دليل
    [directory_not_empty] الدليل غير فارغ
    [read_only_filesystem] نظام الملفات للقراءة فقط
    [stale_network_file_handle] مقبض ملف الشبكة منتهي الصلاحية
    [invalid_input] إدخال العملية غير صالح
    [invalid_data] البيانات غير صالحة
    [timed_out] انتهت مهلة العملية
    [write_zero] لم تحقق الكتابة أي تقدم
    [storage_full] مساحة التخزين ممتلئة
    [not_seekable] لا يمكن الانتقال داخل الكائن
    [quota_exceeded] تم تجاوز حصة التخزين
    [file_too_large] الملف أكبر مما يدعمه النظام الأساسي
    [resource_busy] المورد مشغول
    [executable_file_busy] الملف التنفيذي مشغول
    [deadlock] ستؤدي العملية إلى توقف متبادل
    [crosses_devices] تعبر العملية أجهزة نظام الملفات
    [too_many_links] روابط نظام الملفات كثيرة جدًا
    [invalid_filename] اسم الملف غير صالح
    [argument_list_too_long] قائمة وسائط نظام التشغيل طويلة جدًا
    [interrupted] قوطعت العملية
    [unsupported] العملية غير مدعومة
    [unexpected_eof] نهاية ملف غير متوقعة
    [out_of_memory] تعذر على نظام التشغيل تخصيص الذاكرة
    [other] خطأ آخر في نظام التشغيل
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [unsupported_prompt_locale] يجب أن تكون auto بأحرف صغيرة تمامًا أو لغة واجهة BCP 47 مدعومة
    [language_policy_term_blank] يجب ألا يكون مصطلح سياسة اللغة فارغًا
    [language_policy_term_surrounding_whitespace] يجب ألا يحتوي مصطلح سياسة اللغة على مسافات طرفية
    [language_policy_term_duplicate] يجب ألا يتكرر مصطلح سياسة اللغة
    [quote_repair_candidates_empty] يجب ألا تكون قائمة مرشحي إصلاح علامات الاقتباس فارغة
    [quote_repair_delimiter_invalid] يجب ألا يكون فاصل إصلاح علامات الاقتباس حرفًا أبجديًا رقميًا أو مسافة أو حرف تحكم
    [quote_repair_pair_duplicate] يجب ألا يتكرر زوج إصلاح علامات الاقتباس
    [quote_repair_delimiter_ambiguous] يجب أن ينتمي فاصل إصلاح علامات الاقتباس إلى زوج واحد فقط
    [language_id_blank] يجب ألا يكون معرّف اللغة فارغًا
    [language_id_surrounding_whitespace] يجب ألا يحتوي معرّف اللغة على مسافات طرفية
    [language_id_uses_underscore] يجب أن يستخدم معرّف اللغة الشرطات بين الوسوم الفرعية
    [language_id_invalid_syntax] يجب أن يطابق معرّف اللغة صياغة RFC 5646
    [language_id_invalid_registry_tag] يحتوي معرّف اللغة على وسم فرعي غير صالح في السجل
    [language_id_canonicalization_failed] لا يمكن توحيد معرّف اللغة
    [language_id_undefined_primary_language] يجب أن يحدد معرّف اللغة لغة أساسية
    [language_id_duplicate] يجب أن يكون معرّف اللغة فريدًا
    [language_catalog_empty] يلزم وجود وحدة لغة مصدر واحدة على الأقل
    [url_invalid] يجب أن تكون القيمة عنوان URL صالحًا
    [url_credentials_forbidden] يجب ألا يحتوي URL على بيانات اعتماد
    [url_fragment_forbidden] يجب ألا يحتوي URL على جزء
    [url_scheme_unsupported] يجب أن يكون مخطط URL هو http أو https
    [api_key_blank] يجب ألا يكون API key فارغًا
    [api_key_surrounding_whitespace] يجب ألا يحتوي API key على مسافات طرفية
    [api_key_invalid_header] لا يمكن تمثيل API key كقيمة HTTP Header
    [strict_json_invalid] يجب أن تكون القيمة JSON صارمًا (السطر={ $line }، العمود={ $column })
    [json_object_required] يجب أن تكون القيمة كائن JSON
    [reserved_request_field] هذا الحقل مملوك لبروتوكول الطلب ولا يمكن تجاوزه
    [proxy_must_be_false_or_url] يجب أن يكون proxy هو false أو عنوان http/https كاملاً
    [pem_path_duplicate] يجب أن يكون مسار PEM فريدًا
    [runtime_maximum_exceeded] تتجاوز القيمة الحد الأقصى لوقت التشغيل (الفعلي={ $actual }، الأقصى={ $maximum })
    [value_surrounding_whitespace] يجب ألا تحتوي القيمة على مسافات طرفية
    [value_blank] يجب ألا تكون القيمة فارغة
    [path_blank] يجب ألا يكون المسار فارغًا
    [positive_required] يجب أن تكون القيمة أكبر من صفر (الفعلي={ $actual })
    [usize_range_exceeded] تتجاوز القيمة نطاق usize لهذه المنصة (الفعلي={ $actual })
    [u32_range_exceeded] تتجاوز القيمة نطاق u32 (الفعلي={ $actual })
    [duplicate_profile_id] يجب أن يكون معرّف ملف الترجمة فريدًا
    [selected_profile_invalid] بنية ملف الترجمة المحدد أو أنواع حقوله غير صالحة
    [referenced_client_not_found] عميل LLM المشار إليه غير موجود
   *[other] __ATT_FALLBACK__
}
diagnostic-io-reason = العملية { $operation }: { $kind }
diagnostic-io-reason-with-os-code = العملية { $operation }: { $kind } (OS { $os_code })
diagnostic-io-reason-with-system-message = العملية { $operation }: { $kind }: { $system_message }
diagnostic-io-reason-with-os-code-and-system-message = العملية { $operation }: { $kind } (OS { $os_code }): { $system_message }
diagnostic-failure-with-detail = { $failure }: { $detail }
diagnostic-invalid-utf8 = UTF-8 غير صالح عند البايت { $valid_up_to }، وطول الخطأ { $error_len } بايت
diagnostic-incomplete-utf8 = تسلسل UTF-8 غير مكتمل بعد البايت { $valid_up_to }
diagnostic-toml-failure-value = { $code ->
    [syntax] صياغة TOML غير صالحة
    [missing_field] حقل إعدادات مطلوب مفقود
    [unknown_field] تحتوي الإعدادات على حقل غير معروف
    [duplicate_field] تم تعريف حقل الإعدادات أكثر من مرة
    [type_mismatch] النوع المتوقع هو { $expected }
    [invalid_value] قيمة الإعدادات تخالف عقد الحقل
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] سلسلة نصية
    [integer] عدد صحيح
    [boolean] قيمة منطقية
    [string_or_boolean] سلسلة نصية أو قيمة منطقية
    [string_array] مصفوفة سلاسل نصية
    [integer_array] مصفوفة أعداد صحيحة
    [string_pair_array] مصفوفة أزواج نصية
    [table] جدول
    [table_array] مصفوفة جداول
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = TOML غير صالح ({ $resource }): { $failure }
diagnostic-invalid-toml-at = TOML غير صالح عند السطر { $line } والعمود { $column } ({ $resource }): { $failure }
diagnostic-http-no-details = فشل طلب خدمة النموذج دون تفاصيل عامة عن حالة HTTP
diagnostic-http-status = حالة HTTP ‏{ $status }
diagnostic-http-retry-after = Retry-After بعد { $seconds } ثانية
diagnostic-http-provider-code = رمز خطأ المزوّد { $code }
diagnostic-http-provider-type = نوع خطأ المزوّد { $kind }
diagnostic-http-provider-message = رسالة خطأ المزوّد { $message }
diagnostic-http-fact-separator = ؛
diagnostic-sqlite = رمز خطأ SQLite الأساسي { $primary_code }، ورمز الخطأ الموسّع { $extended_code }
diagnostic-windows-status = فشلت عملية Windows ‏{ $operation } بالحالة NTSTATUS ‏{ $status }
diagnostic-resource = { $resource }: القيمة الفعلية { $actual }
diagnostic-resource-with-maximum = { $resource }: القيمة الفعلية { $actual }، والحد الأقصى { $maximum }
task-record-title = مهمة الترجمة { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] مكتملة
    [partial] مكتملة جزئيًا
    [unavailable] غير متاحة
    [execution_failed] فشل التنفيذ
    [commit_preparation_failed] فشل إعداد التثبيت
    [commit_not_applied] لم يُطبَّق التثبيت
    [commit_outcome_unknown] نتيجة التثبيت غير معروفة
    [not_committed_after_earlier_failure] لم تُثبَّت بعد فشل سابق
    [invalid_result] تسلسل نتائج Executor غير صالح
    [cancelled] ملغاة
   *[other] { $state }
}
task-record-summary-with-written = `المهمة { $ordinal }/{ $total }` · `{ $attempts } محاولات` · `مقبول { $accepted }/{ $expected }` · `كُتب في { $written } مواضع`
task-record-summary-without-written = `المهمة { $ordinal }/{ $total }` · `{ $attempts } محاولات` · `مقبول { $accepted }/{ $expected }`
task-record-run-id-label = معرّف التشغيل:
task-record-started-at-label = وقت البدء:
task-record-duration-label = المدة الإجمالية:
task-record-endpoint-label = نقطة النهاية:
task-record-model-label = النموذج:
task-record-custom-parameters-heading = المعلمات المخصصة
task-record-attempts-heading = محاولات الطلب
task-record-final-result-heading = النتيجة النهائية
task-record-no-request = لم يتكوّن طلب نموذج جاهز للإرسال.
task-record-empty-assistant = أعاد النموذج كائنًا فارغًا.
task-record-parse-error = خطأ في التحليل: { $kind ->
    [json] JSON استجابة النموذج غير صالح (الفئة `{ $category }`)، السطر { $line }، العمود { $column }
    [thinking_not_allowed] وضع الاستجابة الحالي لا يقبل مخرجات التفكير، السطر { $line }، العمود { $column }
    [thinking_envelope_missing] غلاف التفكير المطلوب مفقود، السطر { $line }، العمود { $column }
    [thinking_envelope_unclosed] غلاف التفكير غير مغلق، السطر { $line }، العمود { $column }
    [thinking_empty] محتوى التفكير فارغ، السطر { $line }، العمود { $column }
    [thinking_nested] يوجد غلاف تفكير متداخل، السطر { $line }، العمود { $column }
    [thinking_repeated] يوجد غلاف تفكير متكرر، السطر { $line }، العمود { $column }
    [markdown_fence_no_body] سياج Markdown بلا محتوى، السطر { $line }، العمود { $column }
    [markdown_fence_unsupported] لا يُقبل إلا سياج Markdown واحد بلا وسم لغة أو بوسم json، السطر { $line }، العمود { $column }
    [markdown_fence_unclosed] سياج Markdown غير مغلق، السطر { $line }، العمود { $column }
   *[markdown_fence_invalid_closing] يجب إغلاق سياج Markdown في السطر المستقل الأخير، السطر { $line }، العمود { $column }
}
task-record-attempt-succeeded = المحاولة { $number }: نجحت؛ finish reason { $finish_reason }
task-record-attempt-token-usage = ؛ الرموز `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ؛ المدة `{ $duration }`
task-record-attempt-request-id = ؛ request ID { $request_id }
task-record-attempt-response-id = ؛ response ID { $response_id }
task-record-attempt-retryable = المحاولة { $number }: فشل طلب قابل للإعادة؛ التشخيص `{ $code }`؛ المدة `{ $duration }`
task-record-attempt-retry-after = ؛ Retry-After `{ $duration }`
task-record-attempt-wait-retry = ؛ إعادة المحاولة بعد `{ $duration }`
task-record-attempt-wait-completed = ؛ اكتمل الانتظار لمدة `{ $duration }`؛ لم تبدأ المحاولة التالية
task-record-attempt-wait-cancelled = ؛ كان الانتظار المخطط `{ $duration }`؛ أُلغي أثناء الانتظار
task-record-attempt-failed = المحاولة { $number }: فشل معالجة الطلب أو الاستجابة؛ التشخيص `{ $code }`؛ المدة `{ $duration }`
task-record-attempt-cancelled = المحاولة { $number }: أُلغيت؛ المدة `{ $duration }`
task-record-structured-reason = السبب: { $reason }
task-record-final-status = الحالة: { $state ->
    [complete] مكتملة والتثبيت مؤكّد
    [partial] مكتملة جزئيًا والتثبيت مؤكّد
    [unavailable] غير متاحة؛ المشروع لم يتغير
    [execution_failed] فشل التنفيذ؛ لم تُثبَّت
    [commit_preparation_failed] فشل إعداد التثبيت؛ لم يُطبَّق يقينًا
    [commit_not_applied] المعاملة لم تُطبَّق يقينًا
    [commit_outcome_unknown] نتيجة التثبيت غير معروفة
    [not_committed_after_earlier_failure] لم تُثبَّت بسبب فشل مهمة سابقة
    [invalid_result] تسلسل نتائج Executor غير صالح؛ لم تُثبَّت
    [cancelled] ملغاة؛ لم تُثبَّت
   *[other] { $state }
}
task-record-accepted-written = المقبول: { $accepted } عناصر، كُتبت في { $written } مواضع فعلية
task-record-accepted-outcome-unknown = تم التحقق من { $accepted } عناصر؛ تعذّر تأكيد نتيجة تثبيت قاعدة البيانات
task-record-rejected-heading = غير المقبول:
task-record-rejected-item = { $id }: { $reason }
task-record-protocol-diagnostic = تشخيص البروتوكول: { $diagnostic }
task-record-unavailable-reason = سبب عدم الإتاحة: { $reason }
task-record-task-diagnostic = تشخيص المهمة: `{ $code }`؛ السبب { $reason }
task-record-rejection-reason = { $code ->
    [missing] خرج النموذج مفقود
    [duplicate] خرج النموذج مكرر
    [invalid_shape] { $detail }
    [invalid_shape_array] يجب أن تكون الترجمة مصفوفة من السلاسل
    [invalid_shape_item] يجب أن يكون العنصر { $line } في مصفوفة الترجمة سلسلة
    [line_count_mismatch] عدد الأسطر غير متطابق (المتوقع { $expected }، الفعلي { $actual })
    [invalid_line_text] يحتوي السطر { $line } على محارف تحكم غير صالحة
    [blank_line_mismatch] حالة الفراغ في السطر { $line } غير متطابقة (المتوقع: { $expected_blank ->
        [blank] فارغ
       *[other] غير فارغ
    })
    [blank_translation] الترجمة فارغة
    [no_natural_language_text] لا تحتوي الترجمة على نص بلغة طبيعية
    [contains_byte_order_mark] تحتوي الترجمة على BOM
    [placeholder_mismatch] العنصر النائب غير متطابق: { $detail }
    [unexpected_placeholder] عنصر نائب غير متوقع: { $detail }
    [placeholder_normalization_ambiguous] تطبيع العنصر النائب ملتبس: { $detail }
    [source_residual] اكتُشف نص متبقٍ من لغة المصدر: { $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason ليست stop: { $detail }
    [invalid_response] { $detail }
    [invalid_id] معرّف عنصر النموذج { $index } غير صالح
    [unknown_id] أعاد عنصر النموذج { $index } المعرّف المجهول { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] تعذّر تحليل استجابة النموذج
    [all_outputs_rejected] رُفضت كل مخرجات النموذج عند التحقق
    [recoverable_request_exhausted] استُنفدت ميزانية إعادة الطلبات القابلة للاسترداد
    [retry_after_exceeds_maximum] تتجاوز Retry-After أقصى مدة انتظار مضبوطة
   *[other] { $code }
}
task-record-duration-seconds = { $value } ثانية
task-record-duration-milliseconds = { $value } مللي ثانية
