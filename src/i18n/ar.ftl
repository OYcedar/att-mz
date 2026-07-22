app-about = ترجمة ألعاب RPG Maker باستخدام حالة مشروع قابلة لإعادة الاستخدام
cli-config-help = ملف إعداد TOML صارم لهذا التشغيل
cli-ui-language-help = لغة المساعدة والتشخيص والتقدم والنتائج وسجلات المشروع: ar وzh-Hans وzh-Hant وen وfr وru وes وja وko وvi
cli-progress-help = نمط التقدم المباشر: auto أو plain أو off
cli-mz-about = ترجمة لعبة RPG Maker MZ
cli-mv-about = ترجمة لعبة RPG Maker MV
cli-init-about = تهيئة مشروع لعبة مسمى أو تحديثه
cli-extract-about = استخراج النص بخطة owner صريحة أو محفوظة
cli-translate-about = ترجمة النص المستخرج باستخدام Profile صريح أو محفوظ
cli-write-back-about = كتابة الترجمات المقبولة إلى اللعبة
cli-project-name-help = اسم المشروع الثابت
cli-init-path-help = جذر لعبة RPG Maker؛ يمكن للمشروع الموجود إعادة استخدام آخر مسار ناجح
cli-source-language-help = معرّف لغة المصدر
cli-target-language-help = معرّف اللغة الهدف
cli-dialogue-width-help = الحد الأقصى للمحارف كاملة العرض في سطر الحوار
cli-scrolling-width-help = الحد الأقصى للمحارف كاملة العرض في سطر النص المتمرر
cli-help-width-help = الحد الأقصى للمحارف كاملة العرض في سطر المساعدة أو الوصف
cli-builtin-help = استخدام مواضع نص RPG Maker المضمنة في ATT
cli-rules-help = استبدال owner Rules بتعريف TOML هذا؛ قائمة قواعد فارغة تعطّله
cli-dialogue-rules-help = استبدال إسقاط أسماء حوار MV المستخدم مع Builtin
cli-lua-help = استبدال برنامج Lua للمرحلة؛ ملف حجمه صفر يمسحه
cli-profile-help = معرّف Profile للترجمة؛ يؤدي حذفه إلى إعادة استخدام آخر Profile ناجح
cli-terms-help = استبدال مورد مصطلحات المشروع
cli-placeholders-help = استبدال مورد Placeholder للمشروع
cli-usage-heading = الاستخدام:
cli-commands-heading = الأوامر:
cli-options-heading = الخيارات:
cli-arguments-heading = الوسائط:
cli-options-metavar = خيارات
cli-command-metavar = أمر
cli-print-help = عرض المساعدة
cli-print-version = عرض الإصدار
cli-missing-config = مسار الإعداد المطلوب --config <FILE> مفقود.
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
error-configuration-or-input-generic = الإعداد أو إدخال الأمر غير صالح، لذلك لم يتغير شيء. تحقق من الخيارات وحقول الإعداد المشار إليها ثم أعد المحاولة.
error-config-current-directory-not-absolute = لا يستطيع ATT تحديد --config لأن دليل العمل الحالي { $path } ليس مسارًا مطلقًا. شغّل ATT من دليل صالح أو مرّر مسار إعداد مطلقًا.
error-config-empty-path = لا يجوز أن يكون --config فارغًا. مرّر مسار ملف إعداد ATT.
error-config-open = لا يستطيع ATT فتح ملف الإعداد { $path }. تحقق من وجود الملف ومن صلاحية قراءته.
error-config-not-a-file = مسار الإعداد { $path } ليس ملفًا عاديًا. مرّر ملف إعداد بدلًا من دليل أو ملف خاص.
error-config-too-large = حجم ملف الإعداد { $path } هو { $observed } بايت، والحد { $maximum } بايت. صغّر الملف ثم أعد المحاولة.
error-config-read = لا يستطيع ATT قراءة ملف الإعداد { $path }. تحقق من الصلاحيات وحالة التخزين ثم أعد المحاولة.
error-config-invalid-utf8-known-length = ملف الإعداد { $path } ليس UTF-8 صالحًا. طول البادئة الصالحة { $valid } بايت وطول التسلسل غير الصالح { $length } بايت. احفظ الملف بترميز UTF-8 ثم أعد المحاولة.
error-config-invalid-utf8-unknown-length = ملف الإعداد { $path } ليس UTF-8 صالحًا بعد البايت { $valid }. احفظ الملف بترميز UTF-8 ثم أعد المحاولة.
error-config-invalid-toml-at = يحتوي ملف الإعداد { $path } عند السطر { $line } والعمود { $column } على TOML غير صالح في { $resource }. صحح ذلك القسم وفق عقد الإعداد الحالي ثم أعد المحاولة.
error-config-invalid-toml = يحتوي ملف الإعداد { $path } على TOML غير صالح في { $resource }. صحح ذلك القسم وفق عقد الإعداد الحالي ثم أعد المحاولة.
error-config-invalid-value = قيمة حقل الإعداد { $field } غير صالحة. راجع الحقل في وثائق الإعداد الحالية ثم أعد المحاولة.
error-config-invalid-value-at-path = قيمة حقل الإعداد { $field } في { $path } غير صالحة. راجع الحقل في وثائق الإعداد الحالية ثم أعد المحاولة.
error-config-profile-not-found = لا يحتوي ملف الإعداد { $path } على Profile ترجمة باسم { $profile }. أضفه أو مرّر معرّف Profile موجود.
error-config-profile-conflict = طلب ملف الإعداد { $path } Profile { $requested }، لكن الأمر اختار { $explicit }. وحّد الاختيارين أو أزل أحدهما.
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
log-label-phase-plan-standard = تخطيط الكتابة القياسية
log-label-phase-rewrite-documents = إعادة كتابة المستندات
log-label-phase-validate-candidate = التحقق من المرشح
log-label-task-complete = مكتمل
log-label-task-partial = جزئي
log-label-task-unavailable = غير متاح
log-label-task-failed = فشل
error-project-unavailable = المشروع غير موجود أو مشغول. تحقق من الاسم وأعد المحاولة بعد انتهاء التشغيل الآخر.
error-project-state = حالة المشروع تالفة أو الاستخراج قديم. أعد تشغيل أمر Init أو Extract المعني.
error-external-model = خدمة النموذج غير متاحة. تحقق من Profile والشبكة ثم أعد المحاولة.
error-state-applied-finalization = طُبقت النتيجة لكن الإنهاء فشل. افحص حالة المشروع قبل إعادة المحاولة.
error-outcome-unknown = تعذر تأكيد النتيجة النهائية. احتفظ بمساحة العمل وافحص معلومات الاسترداد قبل إعادة المحاولة.
error-internal = واجه ATT عطلًا داخليًا. لم تُعرض أسرار أو محتويات للنموذج.
error-shutdown = فشل الإنهاء الإلزامي لبيئة التشغيل.
error-no-reusable-extract-plan = لا توجد للمشروع خطة Extract محفوظة. قدّم واحدًا على الأقل من --builtin أو --rules أو --lua.
error-init-path-required = لا يوجد للمشروع مسار مصدر Init محفوظ. قدّم --path <DIR>.
error-profile-required = لا يوجد للمشروع Profile Translate محفوظ. قدّم PROFILE_ID.
error-saved-profile-unavailable = Profile المحفوظ { $profile } غير موجود في الإعداد الحالي. مرّر Profile صالحًا صراحةً.
error-rpg-maker-prompt-unavailable = تعذر استخدام مكوّن موجه RPG Maker { $component } للإعداد المحلي المحدد { $locale } في { $path }، لذلك لم تبدأ الترجمة. تأكد من أن المسار يشير إلى ملف عادي يحتوي على نص UTF-8 غير فارغ، وأن القالب صالح.
error-rpg-maker-language-module-unavailable = وحدة اللغة المصدر المطلوبة للزوج اللغوي { $source } → { $target } مفقودة، لذلك لم تبدأ الترجمة. أضف الوحدة إلى الإعداد الحالي ثم أعد المحاولة.
error-no-executable-extract-owner = لم يبقَ بعد المسح أي owner Extract قابل للتنفيذ، لذلك لم تُحفظ الخطة.
error-plan-save-failed-applied = طُبقت نتيجة الأمر لكن خطة التشغيل الجديدة لم تُحفظ. مرّر الخيارات المطلوبة صراحةً في المرة القادمة.
error-plan-save-outcome-unknown = طُبقت نتيجة الأمر لكن تعذر تأكيد commit خطة التشغيل. مرّر الخيارات المطلوبة صراحةً في المرة القادمة.
plan-source-explicit = إدخال صريح
plan-source-project-state = حالة المشروع
plan-source-product-default = سلوك المنتج
notice-init-reuse-path = لم يُقدّم مسار مصدر؛ سيُعاد استخدام آخر مسار ناجح: { $path }.
notice-extract-reuse-owners = لم يُقدّم نطاق استخراج؛ ستُعاد استخدام آخر خطة ناجحة: { $owners }.
notice-translate-reuse-profile = لم يُقدّم Profile؛ سيُعاد استخدام آخر Profile ناجح: { $profile }.
notice-translate-reuse-lua = لم يُقدّم خيار Lua؛ سيُعاد استخدام آخر اختيار Translate Lua ناجح.
notice-write-back-reuse-lua = لم يُقدّم خيار Lua؛ سيُعاد استخدام آخر برنامج WriteBack Lua ناجح.
notice-write-back-standard-only = لا يوجد برنامج WriteBack Lua معدّ؛ سيُنفذ Standard فقط.
notice-owner-disabled = عُطّل owner { $owner } وأزيل من الخطط التلقائية اللاحقة.
notice-lua-cleared = مُسح برنامج Lua لمرحلة { $phase } ولن يُنفذ هذه المرة.
notice-no-model-request = كل وحدات الترجمة القياسية حديثة؛ لم يرسل Standard طلبًا إلى النموذج هذه المرة.
notice-manual-layout = { $count ->
    [zero] لا توجد وحدات تحتاج إلى مراجعة يدوية لفواصل الأسطر.
    [one] تحتاج وحدة واحدة إلى مراجعة يدوية لفواصل الأسطر.
    [two] تحتاج وحدتان إلى مراجعة يدوية لفواصل الأسطر.
    [few] تحتاج { $count } وحدات إلى مراجعة يدوية لفواصل الأسطر.
    [many] تحتاج { $count } وحدة إلى مراجعة يدوية لفواصل الأسطر.
   *[other] تحتاج { $count } وحدة إلى مراجعة يدوية لفواصل الأسطر.
}
notice-log-degraded = سجل المشروع غير متاح أو متدهور؛ سيستمر الأمر ولن تتغير حالة الخروج.
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
progress-extract-lua = جارٍ تشغيل برنامج Extract Lua
progress-extract-commit = جارٍ تنفيذ commit للأصول المستخرجة
progress-translate-planning = جارٍ تخطيط مهام الترجمة
progress-translate-confirmed = مهام الترجمة المؤكدة
progress-translate-no-work = لا حاجة إلى طلب النموذج
progress-write-back-read-assets = جارٍ قراءة الأصول المقبولة
progress-write-back-planning = جارٍ تخطيط إعادة كتابة المستندات
progress-write-back-documents = المستندات المعاد كتابتها
progress-write-back-lua = جارٍ تشغيل برنامج WriteBack Lua
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
result-translate-standard = الترجمة القياسية: { $total } مهمة؛ مكتملة { $complete }، جزئية { $partial }، غير متاحة { $unavailable }؛ كُتب { $written } موضعًا وتبقى { $remaining }
result-translate-convergence = تقارب الحالة: أُبقي { $retained }، أُبطل { $invalidated }، غير منطبق { $not_applicable }، أُعيد استخدام { $reused }
result-write-back-completed = اكتملت الكتابة: { $project }
result-output-directory = مجلد الإخراج: { $path }
result-write-back-standard = الكتابة القياسية: { $translated } وحدة مترجمة و{ $original } وحدة مصدر؛ التفاف تلقائي { $auto_wrapped }، أضيف { $breaks } فاصل أسطر و{ $indents } إزاحة كاملة العرض؛ يحتاج { $manual } إلى تخطيط يدوي
result-lua-executed = Lua: نُفذ
result-lua-not-executed = Lua: لم يُنفذ
result-cancelled = أُلغي الأمر بعد إنهاء آمن.
result-plan-saved = حُفظت خطة التشغيل الناجحة.
result-translate-plan-sources = حُفظت خطة التشغيل الناجحة الحالية. مصدر Profile: { $profile_source }؛ مصدر Lua: { $lua_source }.
log-run-started = بدأ الأمر { $command }.
log-run-succeeded = اكتمل الأمر { $command } بنجاح.
log-run-failed = فشل الأمر { $command }.
log-run-cancelled = أُلغي الأمر { $command }.
log-plan-resolved = حُلّت خطة الأمر { $command } من { $source }.
log-translate-plan-resolved = حُلّت خطة تشغيل Translate: مصدر Profile هو { $profile_source }، ومصدر Lua هو { $lua_source }.
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
log-partial-result = { $count ->
    [zero] لا توجد نتائج جزئية تحتاج إلى انتباه.
    [one] توجد نتيجة جزئية واحدة تحتاج إلى انتباه.
    [two] توجد نتيجتان جزئيتان تحتاجان إلى انتباه.
    [few] توجد { $count } نتائج جزئية تحتاج إلى انتباه.
    [many] توجد { $count } نتيجة جزئية تحتاج إلى انتباه.
   *[other] توجد { $count } نتيجة جزئية تحتاج إلى انتباه.
}
log-publish-finished = اكتمل نشر الإخراج: { $path }.
log-translation-task-started = بدأت مهمة الترجمة { $index }/{ $total }.
log-translation-task-finished = انتهت مهمة الترجمة { $index } بالنتيجة { $outcome }.
