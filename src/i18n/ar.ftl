app-about = ترجمة الألعاب والنصوص المنظّمة باستخدام حالة مشروع قابلة لإعادة الاستخدام
cli-test-about = فحص إعداد التوزيعة وجميع عملاء LLM
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
cli-ownership-export-about = تصدير ملكية النص لكل وحدة RPG Maker مستخرجة
cli-translation-export-about = تصدير النص المصدر والترجمة الحالية والحالة لكل وحدة مستخرجة
cli-manual-check-about = فحص ملف TOML للترجمات اليدوية دون تعديل المشروع
cli-manual-apply-about = تطبيق الترجمات اليدوية المكتملة والصحيحة
cli-project-lua-about = تشغيل برنامج Lua نصي على قاعدة بيانات المشروع
cli-project-name-help = اسم المشروع الثابت
cli-init-path-help = دليل جذر الإدخال؛ يمكن للمشروع الموجود إعادة استخدام آخر مسار ناجح
cli-source-language-help = معرّف لغة المصدر
cli-target-language-help = معرّف اللغة الهدف
cli-builtin-help = استخدام مواضع نص RPG Maker المضمنة في ATT
cli-rules-help = استبدال قواعد استخراج RPG Maker بتعريف TOML هذا؛ القائمة الفارغة تعطّل القواعد
cli-dialogue-rules-help = استبدال إسقاط أسماء حوار MV المستخدم مع Builtin
cli-profile-help = معرّف Profile للترجمة؛ يؤدي حذفه إلى إعادة استخدام آخر Profile ناجح
cli-terms-help = استبدال مورد مصطلحات المشروع
cli-placeholders-help = استبدال مورد Placeholder للمشروع
cli-project-lua-script-help = برنامج Lua النصي المطلوب تشغيله على قاعدة بيانات المشروع
cli-project-lua-arguments-help = وسيطة UTF-8 تمرر إلى Lua arg[1..] بعد --
cli-manual-file-help = ملف TOML للترجمات اليدوية
cli-jsonl-file-help = ملف تصدير JSONL
cli-retry-rejected-help = إعادة معالجة المرشحات المحفوظة بحالة Rejected
cli-manual-selection-help = نطاق التصدير: pending (الافتراضي) أو rejected أو all
cli-manual-ids-help = تصدير العناصر المطابقة للمعرّفات الطبيعية في ملف JSONL هذا
cli-layout-rules-help = تحميل قواعد تنسيق WriteBack من ملف TOML وحفظها
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
result-translate-summary = الترجمة: خُطط لـ { $total } مهمة، وبدأت { $started }، ولم تبدأ { $not_started }؛ مكتملة { $complete }، جزئية { $partial }، غير متاحة { $unavailable }، فاشلة { $failed }، ملغاة { $cancelled }؛ كُتب { $written } موضعًا وتبقى { $remaining }، منها { $rejected } مرفوضة
result-translate-convergence = تقارب الحالة: أُبقي { $retained }، أُبطل { $invalidated }، غير منطبق { $not_applicable }، أُعيد استخدام { $reused }
result-write-back-completed = اكتملت الكتابة: { $project }
result-project-lua-completed = اكتمل تنفيذ Lua للمشروع: { $project }
result-output-directory = مجلد الإخراج: { $path }
result-write-back-summary = الكتابة: { $translated } وحدة مترجمة و{ $original } وحدة مصدر
result-generic-extract-unchanged = لم تتغير مدخلات Generic: ‏{ $files } ملفًا و{ $groups } مجموعة و{ $units } وحدة
result-generic-extract-updated = حُدثت مدخلات Generic: ‏{ $files } ملفًا و{ $groups } مجموعة و{ $units } وحدة؛ حُفظت { $preserved } ترجمة ومُسحت { $cleared }
result-generic-translate-summary = ترجمة Generic: خُطط لـ { $total } مهمة، وبدأت { $started }، ولم تبدأ { $not_started }؛ مكتملة { $complete }، جزئية { $partial }، غير متاحة { $unavailable }، فاشلة { $failed }، ملغاة { $cancelled }؛ وحدات مخططة { $planned_units }، ومتبقية { $remaining_units }، منها { $rejected_units } مرفوضة، ومُسحت { $cleared }، وأُعيد استخدام { $reused }، وقُبل { $accepted }، وكُتب { $written }، والتعارضات { $conflicted }، ومشكلات الاستجابة { $problems }
result-generic-write-back-summary = كتابة Generic: ‏{ $translated } وحدة مترجمة مع الاحتفاظ بـ { $original } وحدة مصدر
result-run-log = سجل التشغيل: { $path }
result-test-configuration = الإعداد: { $status ->
    [passed] ناجح
   *[failed] فشل
}
result-test-client = LLM { $client }: { $status ->
    [passed] ناجح
   *[failed] فشل
} ({ $protocol }، { $stream ->
    [streaming] تدفق
   *[non_streaming] استجابة كاملة
})
result-test-summary = الملخص: نجح { $passed }/{ $total }، فشل { $failed }، لم يُشغّل { $skipped }
translate-incomplete-object = تشغيل Translate للمشروع { $project }
translate-incomplete-rpg-maker-reason = مهام جزئية: { $partial }، وغير متاحة: { $unavailable }، ولم تبدأ: { $not_started }، ومشكلات بروتوكول: { $protocol }، وطلبات مستنفدة: { $exhausted }؛ قبول الطلبات {
    $admission ->
        [stopped] متوقف
       *[open] مستمر
    }؛ تبقى { $remaining_decisions } قرارات و{ $remaining_locations } مواضع، منها { $rejected_locations } مرفوضة
translate-incomplete-generic-reason = مهام جزئية: { $partial }، وغير متاحة: { $unavailable }، ولم تبدأ: { $not_started }، وطلبات مستنفدة: { $exhausted }؛ قبول الطلبات {
    $admission ->
        [stopped] متوقف
       *[open] مستمر
    }؛ ووحدات متبقية: { $remaining_units }، منها { $rejected_units } مرفوضة، وتعارضات كتابة: { $conflicted }، ومشكلات استجابة: { $problems }
translate-incomplete-help = راجع تشخيصات المهام في سجل هذا التشغيل، وأصلح المشكلات القابلة للتكرار ثم شغّل Translate مجددًا؛ استخدم Manual للباقي القليل
translate-incomplete-rejected-help = راجع تشخيصات المهام؛ أعد محاولة المحتوى المرفوض باستخدام --retry-rejected، أو صدّره عبر manual export --selection rejected لمعالجته من خلال Manual
result-cancelled = أُلغي الأمر بعد إنهاء آمن.
result-plan-saved = حُفظت خطة التشغيل الناجحة.
log-run-started = بدأ الأمر { $command }.
log-run-succeeded = اكتمل الأمر { $command } بنجاح.
log-run-failed = فشل الأمر { $command }.
log-run-outcome-unknown = انتهى الأمر { $command } لكن النتيجة النهائية غير معروفة؛ اتبع التشخيص قبل إعادة المحاولة.
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
log-translation-task-finished = انتهت مهمة الترجمة { $index } بالنتيجة { $outcome }. { $provider_status ->
    [present] المزوّد الأعلى: { $provider }.
   *[missing] المزوّد الأعلى: لم يُقدَّم.
}
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
log-phase-name = { $phase ->
    [check_project] فحص المشروع
    [scan_source] فحص ملفات المصدر
    [prepare_candidate] إعداد النسخة المرشحة
    [update_database] تحديث قاعدة البيانات
    [publish] النشر
    [builtin] استخراج Builtin
    [builtin_documents] فحص مستندات Builtin
    [builtin_work_units] استخراج وحدات النص Builtin
    [builtin_commit] تثبيت نتائج Builtin
    [rules] استخراج Rules
    [rules_documents] فحص مستندات Rules
    [rules_matches] مطابقة Rules
    [rules_commit] تثبيت نتائج Rules
    [lua] تنفيذ Lua
    [planning] تخطيط مهام الترجمة
    [confirmed_tasks] تأكيد مهام الترجمة
    [read_assets] قراءة محتوى المشروع
    [plan_rpg_maker_write_back] تخطيط WriteBack
    [rewrite_documents] إعادة كتابة المستندات
    [validate_candidate] التحقق من النسخة المرشحة
   *[other] __ATT_FALLBACK__
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
    [review_translation] راجع الترجمة المشار إليها؛ استخدم Manual لتصحيحها عند الحاجة
    [review_disabled_rules] إذا كانت هذه النتيجة متوقعة فلا يلزم إجراء؛ وإلا فأضف قواعد صالحة إلى الملف المشار إليه ثم شغّل Extract مجددًا
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
    [empty_text_capture] التقاط text فارغ
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
    [not_regular_file] الهدف الموجود ليس ملفًا عاديًا
    [wrong_publisher_instance] رمز النشر يخص مثيل ناشر آخر
    [journal_corrupt] سجل استرداد النشر غير صالح أو غير مكتمل
    [unexpected_artifact] عنصر غير متوقع في نظام الملفات يمنع العملية
    [interactive_session_already_open] جلسة SQLite تفاعلية أخرى نشطة بالفعل
    [backup_incomplete] لم تصل نسخة SQLite الاحتياطية إلى حالة الاكتمال
    [request_serialization_failed] تعذر إجراء تسلسل لطلب النموذج
    [http_client_build_failed] تعذر إنشاء عميل HTTP لخدمة النموذج
    [dns_resolution_failed] فشل تحليل DNS
    [tcp_connection_failed] فشل اتصال TCP
    [request_send_failed] تعذر إرسال طلب HTTP
    [response_read_failed] تعذرت قراءة استجابة HTTP
    [tls_handshake_failed] فشلت مصافحة TLS
    [connect_timed_out] انتهت مهلة اتصال TCP
    [read_timed_out] انتهت مهلة قراءة استجابة HTTP
    [request_timed_out] تجاوز طلب HTTP المهلة الإجمالية
    [response_decode_failed] تعذر فك ترميز استجابة HTTP
    [redirect_rejected] رُفضت إعادة توجيه HTTP
    [response_parsing_failed] استجابة النموذج ليست JSON صالحًا
    [model_stream_invalid_json] حدث تدفق النموذج ليس JSON صالحًا
    [model_stream_invalid_utf8] يحتوي تدفق النموذج على UTF-8 غير صالح
    [model_stream_error_event] أعاد تدفق النموذج حدث خطأ من الخدمة
    [model_stream_unclosed_event] لم يُغلق حدث SSE بسطر فارغ
    [model_stream_missing_finish] يفتقد تدفق Chat إلى finish_reason
    [model_stream_missing_responses_terminal] يفتقد تدفق Responses إلى حدث نهائي
    [model_stream_event_type_mismatch] لا يتطابق اسم حدث SSE مع نوع JSON
    [model_stream_duplicate_choice] كرر تدفق النموذج choice نفسه
    [model_stream_choice_after_finish] أرسل تدفق Chat حقولًا تغيّر الاستجابة بعد finish
    [model_stream_unexpected_done] أعاد تدفق Responses علامة [DONE] غير متوقعة
    [response_json_invalid] استجابة Assistant ليست JSON صالحًا
    [response_shape_invalid] بنية الجذر أو الاستجابة في JSON الخاص بـ Assistant غير صالحة
    [response_id_invalid] يحتوي عنصر استجابة على output ID غير صالح
    [response_id_unexpected] تحتوي الاستجابة على output ID لم يُطلب
    [response_id_duplicate] تحتوي الاستجابة على output ID نفسه أكثر من مرة
    [response_id_missing] تفتقد الاستجابة output ID مطلوبًا
    [response_translation_not_array] يجب أن تكون translation مصفوفة من السلاسل النصية
    [response_translation_item_not_string] أحد عناصر مصفوفة translation ليس سلسلة نصية
    [response_echo_shape_invalid] لا يطابق كائن source المعاد بنية source/translation المطلوبة
    [response_echo_source_item_not_string] أحد عناصر مصفوفة source المعادة ليس سلسلة نصية
    [response_translation_blank] الترجمة المعادة فارغة
    [response_translation_text_invalid] تحتوي الترجمة المعادة على فاصل أسطر أو NUL أو علامة ترتيب بايتات غير مسموح بها
    [response_placeholder_snapshot_invalid] لقطة Placeholder المستخدمة للتحقق من الاستجابة غير صالحة
    [response_placeholder_identity_or_count_mismatch] غيّرت الترجمة هويات Placeholders المطلوبة أو أعدادها
    [response_placeholder_missing] تفتقد الترجمة token تحكم مطلوبًا
    [response_placeholder_unexpected] تحتوي الترجمة على token تحكم غير متوقع
    [response_placeholder_order_mismatch] غيّرت الترجمة ترتيب tokens التحكم المطلوب
    [response_placeholder_binding_mismatch] غيّرت الترجمة ارتباط Placeholders المطلوبة بالنص
    [response_placeholder_boundary_mismatch] أضافت الترجمة حد Placeholder مطلوبًا أو أزالته
    [response_placeholder_reserved_token] تحتوي الترجمة على token Placeholder محجوز
    [response_placeholder_ambiguous] لا يمكن مطابقة Placeholder المعاد مع token مطلوب واحد بشكل لا لبس فيه
    [response_control_token_invalid] بنية tokens التحكم المعادة غير صالحة
    [response_text_segment_count_mismatch] غيّرت الاستجابة عدد مقاطع النص المطلوبة
    [response_text_segment_shape_mismatch] غيّرت الاستجابة بنية مقاطع النص المطلوبة
    [response_line_count_mismatch] عدد عناصر مصفوفة translation غير صحيح
    [response_line_text_invalid] يحتوي عنصر في مصفوفة translation على نص لا يمكن قبوله
    [response_blank_line_mismatch] لم تحافظ مصفوفة translation على الخانات الفارغة وغير الفارغة المطلوبة
    [response_source_residual] لا تزال الترجمة المقبولة تحتوي على نص بلغة المصدر وتحتاج إلى مراجعة
    [response_finish_requires_review] توقف النموذج لسبب غير نهائي؛ تحتاج النتيجة المعادة إلى مراجعة
    [response_thinking_empty] حقل think المطلوب فارغ أو يحتوي على مسافات بيضاء فقط
    [response_no_usable_output] لا تحتوي استجابة Assistant على إخراج قابل للاستخدام
    [response_all_outputs_rejected] رُفضت كل المخرجات في استجابة Assistant
    [invalid_response_contract] استجابة النموذج لا تستوفي عقد الاستجابة المطلوب
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
diagnostic-http-route-direct = اتصال مباشر (بلا وكيل)
diagnostic-http-route-proxy = عبر الوكيل الصريح { $proxy }
diagnostic-retry-after = Retry-After: ‏{ $seconds } ثانية
diagnostic-provider-code = رمز المزوّد: { $code }
diagnostic-provider-type = نوع المزوّد: { $kind }
diagnostic-provider-message = رسالة المزوّد: { $message }
diagnostic-json-position = السطر { $line }، العمود { $column }
diagnostic-input-field = الحقل: { $field }
diagnostic-input-failure = { $code ->
    [syntax] صياغة TOML غير صالحة
    [missing_field] حقل مطلوب مفقود
    [unknown_field] هذا الحقل غير مسموح به في التنسيق الحالي
    [duplicate_field] الحقل مكرر
    [type_mismatch] نوع الحقل غير صحيح
    [invalid_value] قيمة الحقل غير صالحة
   *[other] __ATT_FALLBACK__
}
diagnostic-expected-type = النوع المطلوب: { $expected ->
    [string] سلسلة نصية
    [integer] عدد صحيح
    [boolean] قيمة منطقية
    [string_or_boolean] سلسلة نصية أو قيمة منطقية
    [string_array] مصفوفة سلاسل نصية
    [integer_array] مصفوفة أعداد صحيحة
    [table] جدول
    [table_array] مصفوفة جداول
    [array] مصفوفة
    [object] كائن
   *[other] __ATT_FALLBACK__
}
diagnostic-response-item = عنصر الاستجابة { $item }
diagnostic-array-item = عنصر المصفوفة { $item }
diagnostic-token-position = موضع token التحكم { $position }
diagnostic-text-segment = مقطع النص { $segment }
diagnostic-post-finish-fields = الحقول بعد finish: { $fields }
diagnostic-expected-actual = المتوقع { $expected }، والمستلم { $actual }
diagnostic-placeholder-rule-file = قاعدة Placeholder رقم { $number } في { $path }
diagnostic-placeholder-rule-project = قاعدة Placeholder رقم { $number } في المشروع الحالي
manual-exported = تم تصدير { $entries } إدخالات إلى { $path }
manual-checked = صالح { $valid }، غير مملوء { $unfilled }، أخطاء { $errors }
manual-applied = طُبّق { $applied }، غير مملوء { $unfilled }، أخطاء { $errors }
manual-value = { $code ->
    [translation_byte_order_mark] يحتوي العنصر { $line } من translation على BOM ‏(U+FEFF)
    [remove_byte_order_mark] أزل المحرف BOM ‏(U+FEFF) من الترجمة
    [keep_placeholders] أعد Placeholder الأصلية إلى الترجمة مع الحفاظ على عددها وترتيبها المطلوب ومواضعها النصية
    [invalid_source_line] يحتوي عنصر source رقم { $line } على سطر جديد أو NUL
    [invalid_translation_line] يحتوي عنصر translation رقم { $line } على سطر جديد أو NUL
    [fixed_length] تتطلب ترجمة fixed عدد { $expected } من العناصر؛ الموجود { $actual }
    [fixed_blank_slot] يجب أن يبقى عنصر ترجمة fixed رقم { $line } فارغًا
    [rerun_export] أعد تشغيل manual export
    [rerun_export_without_controls] أعد تشغيل manual export ولا تضع أسطرًا جديدة أو NUL في عناصر المصفوفة
    [rerun_export_then_fill] أعد تشغيل manual export ثم املأ الترجمة
    [resolve_temporary_then_rerun_export] عالج المسار المؤقت الثابت المعروض، واحذف أي عنصر متبقٍ فيه، ثم أعد تشغيل manual export
    [resolve_published_backup_cleanup] تم تطبيق ملفي التصدير؛ تحقّق منهما ثم احذف ملف backup الثابت المعروض
    [keep_exported_type] احتفظ بقيمة type التي كتبها manual export
   *[other] __ATT_FALLBACK__
}
task-record-title = مهمة الترجمة
task-record-final-result-heading = النتيجة النهائية
task-record-final-status = الحالة: { $state ->
    [complete] مكتملة والتثبيت مؤكّد
    [partial] مكتملة جزئيًا والتثبيت مؤكّد
    [unavailable_rejected_committed] غير متاح؛ تم حفظ المرشحات المرفوضة
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
task-record-provider = المزوّد الأعلى: { $provider }
task-record-provider-unavailable = المزوّد الأعلى: لم يُقدَّم
task-record-requested = الترجمات المطلوبة: { $requested }
task-record-accepted-written = المقبول: { $accepted } عناصر (المعرّفات: { $ids })، كُتبت في { $written } مواضع فعلية
task-record-accepted-outcome-unknown = تم التحقق من { $accepted } عناصر (المعرّفات: { $ids })؛ تعذّر تأكيد نتيجة تثبيت قاعدة البيانات
task-record-unaccepted = غير المقبول: { $unaccepted } عناصر (المعرّفات: { $ids })
task-record-task-diagnostic = تشخيص المهمة
