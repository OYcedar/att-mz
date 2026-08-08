app-about = Traduire des jeux et du texte structuré avec un état de projet réutilisable
cli-ui-language-help = Langue de l’aide, des diagnostics, de la progression, des résultats et des journaux : ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko ou vi
cli-mz-about = Traduire un jeu RPG Maker MZ
cli-mv-about = Traduire un jeu RPG Maker MV
cli-generic-about = Traduire du texte JSONL structuré
cli-init-about = Initialiser ou mettre à jour un projet de traduction nommé
cli-extract-about = Synchroniser le texte source depuis l’entrée actuelle du projet
cli-translate-about = Traduire le texte extrait avec un Profile explicite ou enregistré
cli-write-back-about = Écrire les traductions actuelles dans la sortie du projet
cli-manual-about = Gérer les traductions manuelles dans un fichier TOML modifiable
cli-manual-export-about = Exporter les entrées qui nécessitent une traduction manuelle
cli-manual-check-about = Vérifier un fichier TOML de traductions sans modifier le projet
cli-manual-apply-about = Appliquer les traductions manuelles remplies et valides
cli-project-lua-about = Exécuter un script Lua sur la base de données du projet
cli-project-name-help = Nom stable du projet
cli-init-path-help = Répertoire racine d’entrée ; un projet existant peut réutiliser le dernier chemin réussi
cli-source-language-help = ID de langue source
cli-target-language-help = ID de langue cible
cli-dialogue-width-help = Nombre maximal de caractères pleine chasse par ligne de dialogue
cli-scrolling-width-help = Nombre maximal de caractères pleine chasse par ligne de texte défilant
cli-help-width-help = Nombre maximal de caractères pleine chasse par ligne d’aide ou de description
cli-builtin-help = Utiliser les emplacements de texte RPG Maker intégrés à ATT
cli-rules-help = Remplacer les règles d’extraction RPG Maker par cette définition TOML ; une liste vide les désactive
cli-dialogue-rules-help = Remplacer la projection des noms de dialogue MV utilisée avec Builtin
cli-profile-help = ID du Profile de traduction ; l’omettre réutilise le dernier Profile réussi
cli-terms-help = Remplacer la ressource terminologique du projet
cli-placeholders-help = Remplacer la ressource Placeholder du projet
cli-project-lua-script-help = Script Lua à exécuter sur la base de données du projet
cli-project-lua-arguments-help = Argument UTF-8 transmis à Lua arg[1..] après --
cli-manual-file-help = Fichier TOML de traductions manuelles
cli-usage-heading = Utilisation :
cli-commands-heading = Commandes :
cli-options-heading = Options :
cli-arguments-heading = Arguments :
cli-options-metavar = OPTIONS
cli-command-metavar = COMMANDE
cli-print-help = Afficher l’aide
cli-print-version = Afficher la version
cli-blank-value = La valeur ne peut pas être vide.
cli-invalid-positive-integer = La valeur doit être un entier positif.
cli-invalid-ui-language-argument = --ui-language contient une balise de langue invalide : { $value }.
cli-unsupported-ui-language-argument = --ui-language demande une langue non prise en charge : { $value }.
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE contient une balise de langue invalide : { $value }.
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE demande une langue non prise en charge : { $value }.
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE n’est pas un Unicode valide.
cli-unexpected-argument = Argument inattendu : { $value }.
cli-missing-required-argument = Argument requis absent : { $value }.
cli-invalid-value = La valeur { $value } est invalide pour { $argument }.
cli-error-heading = Erreur :
cli-try-help = Pour plus d’informations, utilisez --help.
cli-missing-value = Une valeur est requise pour { $argument }.
cli-missing-subcommand = Une commande est requise.
cli-argument-conflict = { $argument } ne peut pas être utilisé avec les autres arguments fournis.
cli-wrong-number-of-values = Le nombre de valeurs fourni pour { $argument } est incorrect.
cli-invalid-utf8 = Un argument de ligne de commande n’est pas un Unicode valide.
cli-parse-failure = La ligne de commande n’a pas pu être analysée.
error-no-executable-extract-owner = Après l’effacement, aucun owner Extract n’est exécutable ; le plan n’a donc pas été enregistré.
plan-source-explicit = entrée explicite
plan-source-project-state = état du projet
plan-source-product-default = comportement du produit
notice-init-reuse-path = Aucun chemin source fourni ; réutilisation du dernier chemin réussi : { $path }.
notice-extract-reuse-owners = Aucune portée d’extraction fournie ; réutilisation du dernier plan réussi : { $owners }.
notice-translate-reuse-profile = Aucun Profile fourni ; réutilisation du dernier Profile réussi : { $profile }.
notice-owner-disabled = L’owner { $owner } a été désactivé et retiré des futurs plans automatiques.
warning-rules-command-non-string-skipped = Avertissement : la règle Rules { $rule_number } a ignoré { $skipped_count } paramètres command qui ne sont pas des chaînes (source { $source_file }, code={ $command_code }, parameter={ $parameter }, type={ $actual_type }).
warning-manual-layout-required = Avertissement : vérifiez manuellement les sauts de ligne à { $locations } (region={ $region }, max_fullwidth_chars={ $max_fullwidth_chars }).
notice-no-model-request = Toutes les unités de traduction sont à jour ; cette exécution n’a envoyé aucune requête au modèle.
notice-manual-layout = { $count ->
    [one] 1 unité nécessite une vérification manuelle des sauts de ligne.
   *[other] { $count } unités nécessitent une vérification manuelle des sauts de ligne.
}
notice-log-degraded = La journalisation du projet est indisponible ou dégradée ; la commande continue et son code de sortie ne change pas.
notice-task-records-degraded = Les enregistrements des tâches de traduction sont indisponibles ou dégradés ; la commande continue et son code de sortie ne change pas.
progress-init-check-project = Vérification de l’état du projet
progress-init-scan-source = Analyse de la source du jeu
progress-init-build-candidate = Construction du projet candidat
progress-init-converge-database = Convergence de la base du projet
progress-init-publish = Publication du projet initialisé
progress-save-run-plan = Enregistrement du plan d’exécution réussi
progress-extract-owner = Owner d’extraction : { $owner }
progress-extract-documents = Analyse des documents
progress-extract-builtin = Unités Builtin
progress-extract-rules = Définitions Rules
progress-extract-commit = Commit des ressources extraites
progress-generic-init = Initialisation du projet Generic
progress-generic-extract = Analyse de l’entrée JSONL Generic
progress-translate-planning = Planification des tâches de traduction
progress-translate-confirmed = Tâches de traduction confirmées
progress-translate-no-work = Aucun appel au modèle nécessaire
progress-project-lua = Exécution du programme Lua du projet
progress-write-back-read-assets = Lecture des ressources validées
progress-write-back-planning = Planification de la réécriture des documents
progress-write-back-documents = Documents réécrits
progress-write-back-validate-candidate = Validation du candidat de sortie
progress-write-back-publish = Publication de la sortie ; une interruption attendra un résultat confirmé
progress-finalizing = Finalisation des ressources obligatoires
progress-safe-stopping = Arrêt sécurisé ; conservation de la dernière progression confirmée
result-init-completed = Initialisation terminée : { $project }
result-init-created = État du projet : créé
result-init-unchanged = État du projet : inchangé
result-init-updated = État du projet : mis à jour
result-init-stale-owners = Nouvelle extraction requise : { $owners }
result-extract-completed = Extraction terminée : { $project }
result-translate-completed = Traduction terminée : { $project } (Profile : { $profile })
result-translate-summary = Traduction : { $total } tâches ; { $complete } complètes, { $partial } partielles, { $unavailable } indisponibles ; { $written } emplacements écrits, { $remaining } restants
result-translate-convergence = Convergence : { $retained } conservés, { $invalidated } invalidés, { $not_applicable } non applicables, { $reused } réutilisés
result-write-back-completed = Réécriture terminée : { $project }
result-project-lua-completed = Exécution Lua du projet terminée : { $project }
result-output-directory = Répertoire de sortie : { $path }
result-write-back-summary = Réécriture : { $translated } unités traduites, { $original } unités source ; { $auto_wrapped } retours automatiques, { $breaks } sauts de ligne et { $indents } retraits pleine chasse ajoutés ; { $manual } mises en page manuelles
result-generic-extract-unchanged = Entrée Generic inchangée : { $files } fichiers, { $groups } groupes, { $units } unités
result-generic-extract-updated = Entrée Generic mise à jour : { $files } fichiers, { $groups } groupes, { $units } unités ; { $preserved } traductions conservées et { $cleared } effacées
result-generic-translate-summary = Traduction Generic : { $total } tâches ; { $complete } complètes, { $partial } partielles, { $unavailable } indisponibles ; { $cleared } effacées, { $reused } réutilisées, { $accepted } acceptées, { $written } écrites, { $conflicted } conflits, { $problems } problèmes de réponse
result-generic-write-back-summary = Réécriture Generic : { $translated } unités traduites, { $original } unités source conservées
result-symbol-repair-summary = Réparation des symboles : { $attempted } unités examinées, { $repaired } réparées, { $skipped } ignorées en interne, { $replacements } symboles remplacés
result-cancelled = La commande a été annulée après une finalisation sûre.
result-plan-saved = Le plan d’exécution réussi a été enregistré.
log-run-started = La commande { $command } a démarré.
log-run-succeeded = La commande { $command } s’est terminée avec succès.
log-run-failed = La commande { $command } a échoué.
log-run-outcome-unknown = La commande { $command } s’est terminée avec un résultat final inconnu ; suivez les emplacements de récupération indiqués dans l’erreur.
log-run-cancelled = La commande { $command } a été annulée.
log-performance-counters = Compteurs de performances : { $sqlite_control_attempted_total } tentatives de contrôle de transaction SQLite ; validations complètes de l’arborescence candidate démarrées { $candidate_validation_started }, terminées { $candidate_validation_completed }.
log-lua-print = Lua : { $message }
log-plan-resolved = Le plan de { $command } provient de { $source }.
log-phase-started = Phase démarrée : { $phase }.
log-retry-summary = { $count ->
    [one] 1 nouvelle tentative a été effectuée.
   *[other] { $count } nouvelles tentatives ont été effectuées.
}
log-translation-task-started = Tâche de traduction { $index }/{ $total } démarrée.
log-translation-task-finished = Tâche de traduction { $index } terminée avec le résultat { $outcome }.
log-run-recovery-required = La commande { $command } s’est terminée dans un état nécessitant une récupération ; suivez les emplacements indiqués dans le diagnostic.
log-phase-completed = Phase terminée : { $phase }.
log-phase-stopped = { $outcome ->
    [failed] Échec de la phase : { $phase }.
    [cancelled] Phase annulée : { $phase }.
   *[other] Phase arrêtée : { $phase }.
}
log-cancellation-requested = Annulation demandée après confirmation de { $confirmed } éléments sur { $total }.
log-cancellation-requested-indeterminate = Annulation demandée après confirmation de { $confirmed } éléments ; le total est inconnu.
log-run-plan-finalized = { $result ->
    [saved] Le plan d’exécution a été enregistré.
    [not_saved] Le plan d’exécution n’a pas été enregistré.
    [saved_finalization_failed] Le plan d’exécution a été enregistré, mais la finalisation a échoué.
    [outcome_unknown] L’état final du plan d’exécution est inconnu.
   *[other] La finalisation du plan s’est arrêtée sans résultat reconnu.
}
log-translation-finished = { $result ->
    [not_started] La traduction n’a pas commencé.
    [no_work] La traduction est terminée sans travail nécessaire.
    [complete] La traduction est terminée.
    [incomplete] La traduction est terminée avec du travail restant.
    [failed] La traduction a échoué.
    [cancelled] La traduction a été annulée.
   *[other] La traduction s’est arrêtée sans résultat reconnu.
}
log-publication-started = Publication commencée vers la racine de sortie { $path }.
log-publication-finished = { $result ->
    [published] Publication terminée.
    [not_published] La publication n’a pas modifié la sortie.
    [recovery_required] La publication s’est arrêtée et nécessite une récupération.
    [outcome_unknown] L’état final de la publication est inconnu.
   *[other] La publication s’est arrêtée sans résultat reconnu.
}
log-project-log-degraded = Le journal du projet est dégradé ; { $failure_kinds } catégories d’échec ont été enregistrées.
log-task-outcome-value = { $outcome ->
    [complete] terminée
    [partial] partiellement terminée
    [unavailable] indisponible
    [failed] échouée
    [not_committed_after_earlier_failure] non validée après un échec antérieur
    [cancelled] annulée
   *[other] terminée sans résultat reconnu
}
diagnostic-location = Emplacement : { $subject }
diagnostic-explanation = Cause : { $reason }
diagnostic-resolution = Action : { $action }
diagnostic-related = Erreur associée { $index } :
diagnostic-resolution-value = { $code ->
    [fix_configuration] Corrigez le champ de configuration indiqué, puis réessayez
    [fix_input] Corrigez l’entrée indiquée, puis réessayez
    [fix_placeholder_rules] Corrigez la règle Placeholder indiquée, puis réessayez
    [adjust_manual_layout] Ajustez manuellement les retours à la ligne et la mise en page aux emplacements indiqués selon la largeur d’affichage donnée
    [check_path_and_permissions] Vérifiez le chemin, l’état du système de fichiers et les autorisations
    [check_project_state] Examinez et corrigez l’état du projet, puis réessayez
    [resolve_contention] Attendez la fin de l’opération concurrente, puis réessayez
    [check_model_service] Vérifiez la réponse du service de modèle et les limites du compte
    [preserve_recovery_artifacts] Ne supprimez pas les artefacts de récupération indiqués ; récupérez la sortie avant de réessayer
    [retry] Réessayez l’opération
    [report_bug] Signalez ce défaut ATT et décrivez l’opération en cours
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Une valeur obligatoire est manquante
    [generic_extract_required] L’entrée JSONL ne correspond plus au dernier Extract ; exécutez de nouveau att generic extract
    [conflicting_values] Les valeurs fournies sont incompatibles
    [invalid_syntax] La syntaxe de la valeur est incorrecte
    [invalid_encoding] L’encodage du texte est incorrect
    [invalid_value] La valeur ne respecte pas le contrat requis
    [not_found] L’objet requis n’existe pas
    [state_mismatch] L’état enregistré du projet ne satisfait pas cette opération
    [unsupported_windows_code_page] La page de codes Windows n’est pas UTF-8
    [transaction_rolled_back] La transaction a échoué et ses modifications ont été annulées
    [transaction_outcome_unknown] La transaction s’est terminée sans confirmation de validation ni d’annulation
    [finalization_failed] Le résultat de l’opération existe, mais la finalisation a échoué
    [rollback_failed] L’opération principale et son annulation ont toutes deux échoué
    [external_service_rejected] Le service externe a refusé la requête
    [external_service_unavailable] Le service externe est indisponible
    [executor_closed] Le service d’exécution est en cours d’arrêt ou déjà arrêté
    [concurrent_shutdown] Un autre appelant arrête déjà l’exécuteur
    [executor_state_poisoned] L’état du cycle de vie de l’exécuteur est corrompu
    [worker_spawn_failed] Le système d’exploitation n’a pas pu créer le thread de travail
    [worker_channel_closed] Le canal de commandes du worker s’est fermé avant la fin de la finalisation
    [worker_panicked] Un worker s’est arrêté de façon inattendue
    [reparse_point_forbidden] Le chemin contient un point d’analyse secondaire non fiable
    [non_local_volume] Le chemin ne se trouve pas sur un volume fixe local
    [non_ntfs_volume] Le chemin ne se trouve pas sur un volume NTFS
    [case_sensitive_directory] Le répertoire applique une sémantique de noms sensible à la casse
    [lock_cancelled] L’attente du verrou requis a été annulée
    [target_already_exists] La destination existe déjà
    [file_identity_changed] L’identité du fichier a changé pendant l’opération
    [invalid_path] Le chemin n’est pas une cible valide pour cette opération
    [wrong_publisher_instance] Le jeton de publication appartient à une autre instance de publication
    [journal_corrupt] Le journal de récupération de publication est incorrect ou incomplet
    [unexpected_artifact] Un artefact inattendu du système de fichiers bloque l’opération
    [interactive_session_already_open] Une autre session SQLite interactive est déjà active
    [backup_incomplete] La sauvegarde SQLite n’a pas atteint l’état terminé
    [request_serialization_failed] La requête du modèle n’a pas pu être sérialisée
    [response_parsing_failed] La réponse du modèle n’est pas un JSON valide
    [invalid_response_contract] La réponse du modèle ne respecte pas le contrat de réponse requis
    [transport_failed] Le transport HTTP a échoué avant l’arrivée d’une réponse valide
    [lua_compilation_failed] Le programme Lua principal n’a pas pu être compilé
    [lua_execution_failed] Le programme Lua principal a échoué pendant son exécution
    [rules_pattern_match_failed] Le motif PCRE2 de Rules n’a pas pu être évalué
    [rules_zero_width_match] Le motif Rules a produit une correspondance de largeur nulle
    [rules_overlapping_capture] Le motif Rules a produit des captures de texte qui se chevauchent
    [rules_missing_text_capture] La capture de texte nommée requise n’a pas participé à la correspondance
    [rules_invalid_capture_range] La correspondance ou la capture Rules est hors des limites de caractères UTF-8 valides
    [write_back_candidate_invalid] Le candidat de réécriture ne respecte pas l’arborescence data/js requise
    [write_back_recovery_required] Le répertoire de sortie doit être récupéré avant que son contenu soit fiable
    [already_exists] L’objet cible existe déjà
    [cancelled] L’opération a été annulée
    [concurrent_modification] L’état du projet a été modifié simultanément
    [duplicate_identifier] Un identifiant est dupliqué
    [extraction_out_of_date] L’extraction enregistrée ne correspond plus à la source actuelle
    [invalid_content] Le contenu ne respecte pas le contrat requis
    [manual_layout_required] Un ajustement manuel des sauts de ligne ou de la mise en page est requis
    [operation_failed] L’opération a échoué
    [placeholder_projection_failed] La projection des Placeholder n’a pas conservé la structure requise
    [profile_not_found] Le Profile de traduction sélectionné n’existe pas
    [recovery_required] Une récupération est requise avant de pouvoir faire confiance au résultat
    [resource_limit] Une limite de ressource requise a été atteinte
    [resource_limit_exceeded] L’opération a dépassé une limite de ressource du service
    [source_snapshot_mismatch] La source ne correspond plus à l’instantané enregistré
    [unavailable] Le travail demandé est temporairement indisponible
    [internal_invariant] Un invariant interne a été violé ; il s’agit d’un défaut ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] Le terme de politique linguistique ne doit pas être vide
    [language_policy_term_surrounding_whitespace] Le terme de politique linguistique ne doit pas contenir d’espaces en début ou fin
    [language_policy_term_duplicate] Le terme de politique linguistique ne doit pas être dupliqué
    [language_id_blank] L’identifiant de langue ne doit pas être vide
    [language_id_surrounding_whitespace] L’identifiant de langue ne doit pas contenir d’espaces en début ou fin
    [language_id_uses_underscore] L’identifiant de langue doit séparer les sous-étiquettes par des tirets
    [language_id_invalid_syntax] L’identifiant de langue doit respecter la syntaxe RFC 5646
    [language_id_invalid_registry_tag] L’identifiant de langue contient une sous-étiquette de registre incorrecte
    [language_id_canonicalization_failed] L’identifiant de langue ne peut pas être normalisé
    [language_id_undefined_primary_language] L’identifiant de langue doit définir une langue principale
    [language_id_duplicate] L’identifiant de langue doit être unique
    [language_catalog_empty] Au moins un module de langue source est requis
    [url_invalid] La valeur doit être une URL valide
    [url_credentials_forbidden] L’URL ne doit pas contenir d’identifiants
    [url_fragment_forbidden] L’URL ne doit pas contenir de fragment
    [url_scheme_unsupported] Le schéma de l’URL doit être http ou https
    [api_key_blank] L’API key ne doit pas être vide
    [api_key_surrounding_whitespace] L’API key ne doit pas contenir d’espaces en début ou fin
    [api_key_invalid_header] L’API key ne peut pas être représentée comme valeur HTTP Header
    [strict_json_invalid] La valeur doit être un JSON strict (ligne={ $line }, colonne={ $column })
    [json_object_required] La valeur doit être un objet JSON
    [reserved_request_field] Le champ appartient au protocole de requête et ne peut pas être remplacé
    [proxy_must_be_false_or_url] proxy doit être false ou une URL http/https complète
    [pem_path_duplicate] Le chemin PEM doit être unique
    [runtime_maximum_exceeded] La valeur dépasse le maximum de l’environnement (valeur réelle={ $actual }, maximum={ $maximum })
    [value_surrounding_whitespace] La valeur ne doit pas contenir d’espaces en début ou fin
    [value_blank] La valeur ne doit pas être vide
    [path_blank] Le chemin ne doit pas être vide
    [positive_required] La valeur doit être supérieure à zéro (valeur réelle={ $actual })
    [usize_range_exceeded] La valeur dépasse la plage usize de cette plateforme (valeur réelle={ $actual })
    [u32_range_exceeded] La valeur dépasse la plage u32 (valeur réelle={ $actual })
    [duplicate_profile_id] L’identifiant du profil de traduction doit être unique
    [selected_profile_invalid] La structure ou les types de champs du profil de traduction sélectionné sont incorrects
    [referenced_client_not_found] Le client LLM référencé n’existe pas
   *[other] __ATT_FALLBACK__
}
task-record-title = Tâche de traduction { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] Terminée
    [partial] Partiellement terminée
    [unavailable] Indisponible
    [execution_failed] Échec d’exécution
    [commit_preparation_failed] Échec de préparation du commit
    [commit_not_applied] Commit non appliqué
    [commit_outcome_unknown] Résultat du commit inconnu
    [not_committed_after_earlier_failure] Non validée après un échec antérieur
    [invalid_result] Séquence de résultats Executor invalide
    [cancelled] Annulée
   *[other] { $state }
}
task-record-summary-with-written = `Tâche { $ordinal }/{ $total }` · `{ $attempts } tentatives` · `Acceptées { $accepted }/{ $expected }` · `Écrites à { $written } emplacements`
task-record-summary-without-written = `Tâche { $ordinal }/{ $total }` · `{ $attempts } tentatives` · `Acceptées { $accepted }/{ $expected }`
task-record-run-id-label = ID d’exécution :
task-record-started-at-label = Début :
task-record-duration-label = Durée totale :
task-record-endpoint-label = Endpoint :
task-record-model-label = Modèle :
task-record-custom-parameters-heading = Paramètres personnalisés
task-record-attempts-heading = Tentatives de requête
task-record-final-result-heading = Résultat final
task-record-no-request = Aucune requête de modèle prête à être envoyée.
task-record-empty-assistant = Le modèle a renvoyé un objet vide.
task-record-parse-error = Erreur d’analyse : { $kind ->
    [thinking_empty] le contenu du raisonnement est vide, ligne { $line }, colonne { $column }
   *[json] JSON de réponse du modèle invalide (catégorie `{ $category }`), ligne { $line }, colonne { $column }
}
task-record-attempt-succeeded = Tentative { $number } : réussie ; finish reason { $finish_reason }
task-record-attempt-token-usage = ; tokens `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; durée `{ $duration }`
task-record-attempt-retryable = Tentative { $number } : échec réessayable ; durée `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; nouvelle tentative après `{ $duration }`
task-record-attempt-wait-completed = ; attente de `{ $duration }` terminée ; tentative suivante non démarrée
task-record-attempt-wait-cancelled = ; attente prévue de `{ $duration }` ; annulation pendant l’attente
task-record-attempt-failed = Tentative { $number } : échec de traitement de la requête ou réponse ; durée `{ $duration }`
task-record-attempt-cancelled = Tentative { $number } : annulée ; durée `{ $duration }`
task-record-final-status = État : { $state ->
    [complete] terminée, commit confirmé
    [partial] partiellement terminée, commit confirmé
    [unavailable] indisponible, projet inchangé
    [execution_failed] échec d’exécution, non validée
    [commit_preparation_failed] échec de préparation du commit, non appliqué avec certitude
    [commit_not_applied] transaction non appliquée avec certitude
    [commit_outcome_unknown] résultat du commit inconnu
    [not_committed_after_earlier_failure] non validée à cause de l’échec d’une tâche antérieure
    [invalid_result] séquence de résultats Executor invalide, non validée
    [cancelled] annulée, non validée
   *[other] { $state }
}
task-record-accepted-written = Acceptées : { $accepted } entrées, écrites à { $written } emplacements réels
task-record-accepted-outcome-unknown = Validées : { $accepted } entrées ; résultat du commit de base de données impossible à confirmer
task-record-task-diagnostic = Diagnostic de tâche
task-record-duration-seconds = { $value } secondes
task-record-duration-milliseconds = { $value } ms
