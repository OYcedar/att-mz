app-about = ترجمة الألعاب والنصوص المنظّمة باستخدام حالة مشروع قابلة لإعادة الاستخدام
cli-ui-language-help = لغة المساعدة والتشخيص والتقدم والنتائج وسجلات المشروع: ar وzh-Hans وzh-Hant وen وfr وru وes وja وko وvi
cli-mz-about = ترجمة لعبة RPG Maker MZ
cli-mv-about = ترجمة لعبة RPG Maker MV
cli-generic-about = ترجمة نص JSONL منظّم
cli-init-about = تهيئة مشروع ترجمة مسمى أو تحديثه
cli-extract-about = مزامنة النص المصدر من مدخل المشروع الحالي
cli-translate-about = ترجمة النص المستخرج باستخدام Profile صريح أو محفوظ
cli-write-back-about = كتابة الترجمات الحالية إلى مخرجات المشروع
cli-manual-about = إدارة الترجمات اليدوية في ملف TOML قابل للتحرير
cli-manual-export-about = تصدير العناصر التي تحتاج حاليًا إلى ترجمة يدوية
cli-manual-check-about = فحص ملف TOML للترجمات اليدوية دون تعديل المشروع
cli-manual-apply-about = تطبيق الترجمات اليدوية المكتملة والصحيحة
cli-project-lua-about = تشغيل برنامج Lua نصي على قاعدة بيانات المشروع
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
cli-project-lua-script-help = برنامج Lua النصي المطلوب تشغيله على قاعدة بيانات المشروع
cli-project-lua-arguments-help = وسيطة UTF-8 تمرر إلى Lua arg[1..] بعد --
cli-manual-file-help = ملف TOML للترجمات اليدوية
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
plan-source-explicit = إدخال صريح
plan-source-project-state = حالة المشروع
plan-source-product-default = سلوك المنتج
notice-init-reuse-path = لم يُقدّم مسار مصدر؛ سيُعاد استخدام آخر مسار ناجح: { $path }.
notice-extract-reuse-owners = لم يُقدّم نطاق استخراج؛ ستُعاد استخدام آخر خطة ناجحة: { $owners }.
notice-translate-reuse-profile = لم يُقدّم Profile؛ سيُعاد استخدام آخر Profile ناجح: { $profile }.
notice-no-model-request = كل وحدات الترجمة حديثة؛ لم تحتج هذه الجولة إلى إرسال طلب للنموذج.
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
progress-no-work = لا يوجد عمل مطلوب
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
result-translate-completed = انتهى تشغيل الترجمة: { $project } (Profile: { $profile })
result-translate-status = الحالة: { $status }
result-translate-status-value = { $status ->
    [no_work] لا يوجد عمل
    [complete] مكتمل
    [incomplete] غير مكتمل
   *[other] __ATT_FALLBACK__
}
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
result-symbol-repair-summary = إصلاح الرموز: فُحصت { $attempted } وحدة، وأُصلحت { $repaired }، وتُخطيت داخليًا { $skipped }، واستُبدل { $replacements } رمزًا
result-run-log = سجل التشغيل: { $path }
translate-incomplete-object = تشغيل Translate للمشروع { $project }
translate-incomplete-rpg-maker-reason = مهام جزئية: { $partial }، وغير متاحة: { $unavailable }، ومشكلات بروتوكول: { $protocol }، وطلبات مستنفدة: { $exhausted }؛ تبقى { $remaining_decisions } قرارات و{ $remaining_locations } مواضع
translate-incomplete-generic-reason = مهام جزئية: { $partial }، وغير متاحة: { $unavailable }، وتعارضات كتابة: { $conflicted }، ومشكلات استجابة: { $problems }
translate-incomplete-help = راجع تشخيصات المهام في سجل هذا التشغيل، وأصلح المشكلات القابلة للتكرار ثم شغّل Translate مجددًا؛ استخدم Manual للباقي القليل
result-cancelled = أُلغي الأمر بعد إنهاء آمن.
result-plan-saved = حُفظت خطة التشغيل الناجحة.
log-run-started = بدأ الأمر { $command }.
log-run-succeeded = اكتمل الأمر { $command } بنجاح.
log-run-failed = فشل الأمر { $command }.
log-run-outcome-unknown = انتهى الأمر { $command } لكن النتيجة النهائية غير معروفة؛ اتبع مواقع الاسترداد الواردة في الخطأ.
log-run-cancelled = أُلغي الأمر { $command }.
log-performance-counters = عدادات الأداء: محاولات التحكم في معاملات SQLite‏ { $sqlite_control_attempted_total }؛ بدء التحقق الكامل من شجرة المرشح { $candidate_validation_started }، واكتماله { $candidate_validation_completed }.
log-lua-print = Lua: { $message }
log-plan-resolved = حُلّت خطة الأمر { $command } من { $source }.
log-phase-started = بدأت المرحلة: { $phase }.
log-retry-summary = { $count ->
    [zero] لا توجد محاولات إعادة.
    [one] نُفذت محاولة واحدة.
    [two] نُفذت محاولتان.
    [few] نُفذت { $count } محاولات.
    [many] نُفذت { $count } محاولة.
   *[other] نُفذت { $count } محاولة.
}
log-translation-task-started = بدأت مهمة الترجمة { $index }/{ $total }.
log-translation-task-finished = انتهت مهمة الترجمة { $index } بالنتيجة { $outcome }.
log-run-recovery-required = انتهى الأمر { $command } بحالة تتطلب الاسترداد؛ اتبع مواقع الاسترداد الواردة في التشخيص.
log-phase-completed = اكتملت المرحلة: { $phase }.
log-phase-stopped = { $outcome ->
    [failed] فشلت المرحلة: { $phase }.
    [cancelled] أُلغيت المرحلة: { $phase }.
   *[other] توقفت المرحلة: { $phase }.
}
log-cancellation-requested = طُلب الإلغاء بعد تأكيد { $confirmed } من أصل { $total } عناصر.
log-cancellation-requested-indeterminate = طُلب الإلغاء بعد تأكيد { $confirmed } عناصر؛ العدد الإجمالي غير معروف.
log-run-plan-finalized = { $result ->
    [saved] حُفظت خطة التشغيل.
    [not_saved] لم تُحفظ خطة التشغيل.
    [saved_finalization_failed] حُفظت خطة التشغيل، لكن فشلت عملية الإنهاء.
    [outcome_unknown] الحالة النهائية لخطة التشغيل غير معروفة.
   *[other] توقفت عملية إنهاء الخطة دون نتيجة معروفة.
}
log-translation-finished = { $result ->
    [not_started] لم تبدأ الترجمة.
    [no_work] انتهت الترجمة دون عمل مطلوب.
    [complete] اكتملت الترجمة.
    [incomplete] انتهت الترجمة مع بقاء عمل غير مكتمل.
    [failed] فشلت الترجمة.
    [cancelled] أُلغيت الترجمة.
   *[other] توقفت الترجمة دون نتيجة معروفة.
}
log-publication-started = بدأ النشر إلى جذر الإخراج { $path }.
log-publication-finished = { $result ->
    [published] اكتمل النشر.
    [not_published] لم يغيّر النشر المخرجات.
    [recovery_required] توقف النشر ويتطلب الاسترداد.
    [outcome_unknown] الحالة النهائية للنشر غير معروفة.
   *[other] توقف النشر دون نتيجة معروفة.
}
log-task-outcome-value = { $outcome ->
    [complete] مكتملة
    [partial] مكتملة جزئيًا
    [unavailable] غير متاحة
    [failed] فاشلة
    [not_committed_after_earlier_failure] لم تُثبَّت بعد فشل سابق
    [cancelled] ملغاة
   *[other] انتهت دون نتيجة معروفة
}
diagnostic-object = الكائن: { $subject }
diagnostic-error-heading = خطأ:
diagnostic-warning-heading = تحذير:
diagnostic-explanation = السبب: { $reason }
diagnostic-impact = الأثر: { $impact }
diagnostic-resolution = الإجراء: { $action }
diagnostic-related = { $relation ->
    [cleanup] فشل التنظيف أيضًا:
    [rollback] فشل التراجع أيضًا:
    [discard] فشل التخلص من المرشح أيضًا:
    [finalization] فشل الإنهاء أيضًا:
    [shutdown] فشل الإغلاق أيضًا:
    [observability] فشل عرض النتيجة أو تسجيلها أيضًا:
   *[other] فشلت عملية مرتبطة أيضًا:
}
diagnostic-impact-value = { $effect ->
    [unchanged] لم تتغير حالة العمل
    [progress_preserved] حُفظ التقدم المؤكد سابقًا؛ ولم يكتمل المحتوى المشار إليه
    [applied] أصبحت نتيجة العمل المرتبطة نافذة
    [applied_run_plan_not_saved] أصبحت نتيجة العمل نافذة، لكن خطة هذا التشغيل لم تُحفظ
    [applied_finalization_failed] أصبحت نتيجة العمل نافذة، لكن الإنهاء المطلوب لم يكتمل
    [recovery_required] النتيجة معروفة، لكن يجب معالجة موضع الاسترداد المشار إليه أولًا
    [outcome_unknown] لا يمكن تأكيد ما إذا أصبحت العملية نافذة؛ لا تُعد المحاولة ولا تحذف عناصر الاسترداد قبل اتباع الإجراء
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] صحح حقل الإعدادات المحدد ثم أعد المحاولة
    [fix_input] صحح الإدخال المحدد ثم أعد المحاولة
    [fix_placeholder_rules] صحح قاعدة Placeholder المحددة ثم أعد المحاولة
    [review_disabled_rules] إذا كانت هذه النتيجة متوقعة فلا يلزم إجراء؛ وإلا فأضف قواعد صالحة إلى الملف المشار إليه ثم شغّل Extract مجددًا
    [adjust_manual_layout] اضبط فواصل الأسطر والتخطيط يدويا في المواقع المحددة وفقا لعرض الشاشة المذكور
    [check_path_and_permissions] تحقق من المسار وحالة نظام الملفات والأذونات
    [check_project_state] افحص حالة المشروع وصححها ثم أعد المحاولة
    [resolve_contention] انتظر انتهاء العملية المتعارضة ثم أعد المحاولة
    [check_model_service] تحقق من استجابة خدمة النموذج وحدود الحساب
    [preserve_recovery_artifacts] لا تحذف عناصر الاسترداد المدرجة؛ استرد المخرجات قبل إعادة المحاولة
    [retry] أعد محاولة العملية
    [report_bug] أبلغ عن عيب ATT هذا واشرح العملية التي كنت تنفذها
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] قيمة مطلوبة مفقودة
    [generic_extract_required] لم يعد إدخال JSONL مطابقًا لآخر Extract؛ شغّل att generic extract مرة أخرى
    [conflicting_values] القيم المقدمة متعارضة
    [invalid_syntax] صياغة القيمة غير صالحة
    [invalid_encoding] ترميز النص غير صالح
    [invalid_value] القيمة تخالف العقد المطلوب
    [rules_owner_disabled] يستخدم ملف Rules المحدد rule = []؛ عُطّل Rules وحُذفت أصوله المستخرجة
    [not_found] الكائن المطلوب غير موجود
    [state_mismatch] حالة المشروع المحفوظة لا تستوفي متطلبات هذه العملية
    [unsupported_windows_code_page] صفحة الرموز في Windows ليست UTF-8
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
    [stdout_write_failed] تعذرت الكتابة إلى الإخراج القياسي
    [stderr_write_failed] تعذرت الكتابة إلى الخطأ القياسي
    [stdout_flush_failed] تعذر تفريغ الإخراج القياسي
    [stderr_flush_failed] تعذر تفريغ الخطأ القياسي
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
    [lua_compilation_failed] تعذر تجميع برنامج Lua الرئيسي
    [lua_execution_failed] فشل برنامج Lua الرئيسي أثناء التشغيل
    [rules_pattern_match_failed] تعذر تقييم نمط PCRE2 في Rules
    [rules_zero_width_match] أنتج نمط Rules تطابقًا بعرض صفري
    [rules_overlapping_capture] أنتج نمط Rules لقطات نصية متداخلة
    [rules_missing_text_capture] لم تشارك لقطة النص المسماة المطلوبة في التطابق
    [rules_invalid_capture_range] تطابق Rules أو نطاق اللقطة خارج حدود أحرف UTF-8 الصالحة
    [write_back_candidate_invalid] مرشح إعادة الكتابة لا يستوفي بنية شجرة data/js المطلوبة
    [write_back_recovery_required] يلزم استرداد دليل الإخراج قبل الوثوق بمحتوياته
    [already_exists] الكائن الهدف موجود بالفعل
    [cancelled] أُلغيت العملية
    [concurrent_modification] تغيّرت حالة المشروع بالتزامن مع العملية
    [duplicate_identifier] يوجد معرّف مكرر
    [extraction_out_of_date] لم يعد الاستخراج المحفوظ يطابق المصدر الحالي
    [invalid_content] لا يتوافق المحتوى مع العقد المطلوب
    [manual_layout_required] يلزم ضبط فواصل الأسطر أو التخطيط يدوياً
    [operation_failed] فشلت العملية
    [placeholder_projection_failed] لم يحافظ إسقاط Placeholder على البنية المطلوبة
    [profile_not_found] Profile الترجمة المحدد غير موجود
    [recovery_required] يلزم الاسترداد قبل الوثوق بالنتيجة
    [resource_limit] تم بلوغ حد مورد مطلوب
    [resource_limit_exceeded] تجاوزت العملية حد موارد في الخدمة
    [source_snapshot_mismatch] لم يعد المصدر يطابق اللقطة المحفوظة
    [unavailable] العمل المطلوب غير متاح مؤقتاً
    [internal_invariant] انتُهك ثابت داخلي؛ هذا عيب في ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] يجب ألا يكون مصطلح سياسة اللغة فارغًا
    [language_policy_term_surrounding_whitespace] يجب ألا يحتوي مصطلح سياسة اللغة على مسافات طرفية
    [language_policy_term_duplicate] يجب ألا يتكرر مصطلح سياسة اللغة
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
diagnostic-http-status = حالة HTTP ‏{ $status }
diagnostic-retry-after = Retry-After: ‏{ $seconds } ثانية
diagnostic-provider-code = رمز المزوّد: { $code }
diagnostic-provider-type = نوع المزوّد: { $kind }
diagnostic-provider-message = رسالة المزوّد: { $message }
diagnostic-json-position = السطر { $line }، العمود { $column }
diagnostic-placeholder-rule-file = قاعدة Placeholder رقم { $number } في { $path }
diagnostic-placeholder-rule-project = قاعدة Placeholder رقم { $number } في المشروع الحالي
manual-exported = تم تصدير { $entries } إدخالات إلى { $path }
manual-checked = صالح { $valid }، غير مملوء { $unfilled }، أخطاء { $errors }
manual-applied = طُبّق { $applied }، غير مملوء { $unfilled }، أخطاء { $errors }
manual-value = { $code ->
    [invalid_source_line] يحتوي عنصر source رقم { $line } على سطر جديد أو NUL
    [invalid_translation_line] يحتوي عنصر translation رقم { $line } على سطر جديد أو NUL
    [fixed_length] تتطلب ترجمة fixed عدد { $expected } من العناصر؛ الموجود { $actual }
    [fixed_blank_slot] يجب أن يبقى عنصر ترجمة fixed رقم { $line } فارغًا
    [rerun_export] أعد تشغيل manual export
    [rerun_export_without_controls] أعد تشغيل manual export ولا تضع أسطرًا جديدة أو NUL في عناصر المصفوفة
    [rerun_export_then_fill] أعد تشغيل manual export ثم املأ الترجمة
    [resolve_temporary_then_rerun_export] عالج المسار المؤقت الثابت المعروض، واحذف أي عنصر متبقٍ فيه، ثم أعد تشغيل manual export
    [keep_exported_type] احتفظ بقيمة type التي كتبها manual export
   *[other] __ATT_FALLBACK__
}
task-record-title = مهمة الترجمة
task-record-final-result-heading = النتيجة النهائية
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
task-record-task-diagnostic = تشخيص المهمة
