app-about = Traduire des jeux et du texte structuré avec un état de projet réutilisable
cli-test-about = Vérifier la configuration de distribution et tous les clients LLM
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
cli-ownership-export-about = Exporter la propriété du texte de chaque unité RPG Maker extraite
cli-translation-export-about = Exporter le texte source, la traduction actuelle et l’état de chaque unité extraite
cli-manual-check-about = Vérifier un fichier TOML de traductions sans modifier le projet
cli-manual-apply-about = Appliquer les traductions manuelles remplies et valides
cli-project-lua-about = Exécuter un script Lua sur la base de données du projet
cli-project-name-help = Nom stable du projet
cli-init-path-help = Répertoire racine d’entrée ; un projet existant peut réutiliser le dernier chemin réussi
cli-source-language-help = ID de langue source
cli-target-language-help = ID de langue cible
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
plan-source-explicit = entrée explicite
plan-source-project-state = état du projet
plan-source-product-default = comportement du produit
notice-init-reuse-path = Aucun chemin source fourni ; réutilisation du dernier chemin réussi : { $path }.
notice-extract-reuse-owners = Aucune portée d’extraction fournie ; réutilisation du dernier plan réussi : { $owners }.
notice-translate-reuse-profile = Aucun Profile fourni ; réutilisation du dernier Profile réussi : { $profile }.
notice-no-model-request = Toutes les unités de traduction sont à jour ; cette exécution n’a envoyé aucune requête au modèle.
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
progress-no-work = Aucun traitement nécessaire
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
result-translate-completed = Exécution de traduction terminée : { $project } (Profile : { $profile })
result-translate-status = État : { $status }
result-translate-status-value = { $status ->
    [no_work] aucun traitement nécessaire
    [complete] complet
    [incomplete] incomplet
   *[other] __ATT_FALLBACK__
}
result-translate-summary = Traduction : { $total } tâches prévues, { $started } commencées, { $not_started } non commencées ; { $complete } complètes, { $partial } partielles, { $unavailable } indisponibles, { $failed } échouées, { $cancelled } annulées ; { $written } emplacements écrits, { $remaining } restants, dont { $rejected } rejetés
result-translate-convergence = Convergence : { $retained } conservés, { $invalidated } invalidés, { $not_applicable } non applicables, { $reused } réutilisés
result-write-back-completed = Réécriture terminée : { $project }
result-project-lua-completed = Exécution Lua du projet terminée : { $project }
result-output-directory = Répertoire de sortie : { $path }
result-write-back-summary = Réécriture : { $translated } unités traduites, { $original } unités source
result-generic-extract-unchanged = Entrée Generic inchangée : { $files } fichiers, { $groups } groupes, { $units } unités
result-generic-extract-updated = Entrée Generic mise à jour : { $files } fichiers, { $groups } groupes, { $units } unités ; { $preserved } traductions conservées et { $cleared } effacées
result-generic-translate-summary = Traduction Generic : { $total } tâches prévues, { $started } commencées, { $not_started } non commencées ; { $complete } complètes, { $partial } partielles, { $unavailable } indisponibles, { $failed } échouées, { $cancelled } annulées ; { $planned_units } unités prévues, { $remaining_units } restantes, dont { $rejected_units } rejetées, { $cleared } effacées, { $reused } réutilisées, { $accepted } acceptées, { $written } écrites, { $conflicted } conflits, { $problems } problèmes de réponse
result-generic-write-back-summary = Réécriture Generic : { $translated } unités traduites, { $original } unités source conservées
result-run-log = Journal d’exécution : { $path }
result-test-configuration = Configuration : { $status ->
    [passed] réussie
   *[failed] échouée
}
result-test-client = LLM { $client } : { $status ->
    [passed] réussi
   *[failed] échoué
} ({ $protocol }, { $stream ->
    [streaming] diffusion continue
   *[non_streaming] réponse complète
})
result-test-summary = Résumé : { $passed }/{ $total } réussis, { $failed } échoués, { $skipped } non exécutés
translate-incomplete-object = Exécution Translate du projet { $project }
translate-incomplete-rpg-maker-reason = { $partial } tâches partielles, { $unavailable } indisponibles, { $not_started } non commencées, { $protocol } problèmes de protocole et { $exhausted } requêtes épuisées ; l’admission des requêtes {
    $admission ->
        [stopped] a été arrêtée
       *[open] est restée ouverte
    } ; il reste { $remaining_decisions } décisions et { $remaining_locations } emplacements, dont { $rejected_locations } rejetés
translate-incomplete-generic-reason = { $partial } tâches partielles, { $unavailable } indisponibles, { $not_started } non commencées, { $exhausted } requêtes épuisées ; l’admission des requêtes {
    $admission ->
        [stopped] a été arrêtée
       *[open] est restée ouverte
    } ; { $remaining_units } unités restantes, dont { $rejected_units } rejetées, { $conflicted } conflits d’écriture et { $problems } problèmes de réponse
translate-incomplete-help = Consultez les diagnostics des tâches dans ce journal, corrigez les problèmes reproductibles puis relancez Translate ; utilisez Manual pour un petit reliquat
translate-incomplete-rejected-help = Consultez les diagnostics des tâches ; relancez les contenus rejetés avec --retry-rejected, ou exportez-les avec manual export --selection rejected pour les traiter via Manual
result-cancelled = La commande a été annulée après une finalisation sûre.
result-plan-saved = Le plan d’exécution réussi a été enregistré.
log-run-started = La commande { $command } a démarré.
log-run-succeeded = La commande { $command } s’est terminée avec succès.
log-run-failed = La commande { $command } a échoué.
log-run-outcome-unknown = La commande { $command } s’est terminée avec un résultat final inconnu ; suivez le diagnostic avant de réessayer.
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
log-task-outcome-value = { $outcome ->
    [complete] terminée
    [partial] partiellement terminée
    [unavailable] indisponible
    [failed] échouée
    [not_committed_after_earlier_failure] non validée après un échec antérieur
    [cancelled] annulée
   *[other] terminée sans résultat reconnu
}
diagnostic-object = Objet : { $subject }
diagnostic-error-heading = Erreur :
diagnostic-warning-heading = Avertissement :
diagnostic-explanation = Cause : { $reason }
diagnostic-impact = Impact : { $impact }
diagnostic-resolution = Action : { $action }
diagnostic-related = { $relation ->
    [cleanup] Le nettoyage a également échoué :
    [rollback] La restauration a également échoué :
    [discard] La suppression du candidat a également échoué :
    [finalization] La finalisation a également échoué :
    [shutdown] L’arrêt a également échoué :
    [observability] La présentation ou l’enregistrement du résultat a également échoué :
   *[other] Une opération associée a également échoué :
}
diagnostic-impact-value = { $effect ->
    [unchanged] L’état métier n’a pas été modifié
    [progress_preserved] La progression précédemment confirmée est conservée ; le contenu indiqué n’est pas terminé
    [applied] Le résultat métier associé a déjà pris effet
    [applied_run_plan_not_saved] Le résultat métier a pris effet, mais le plan de cette exécution n’a pas été enregistré
    [applied_finalization_failed] Le résultat métier a pris effet, mais la finalisation requise n’est pas terminée
    [recovery_required] Le résultat est connu, mais le site de récupération indiqué doit d’abord être traité
    [outcome_unknown] Impossible de confirmer si l’opération a pris effet ; ne réessayez pas et ne supprimez pas les éléments de récupération avant de suivre l’action indiquée
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] Corrigez le champ de configuration indiqué, puis réessayez
    [fix_input] Corrigez l’entrée indiquée, puis réessayez
    [fix_placeholder_rules] Corrigez la règle Placeholder indiquée, puis réessayez
    [review_translation] Vérifiez la traduction indiquée ; utilisez Manual pour la corriger si nécessaire
    [review_disabled_rules] Si ce résultat est attendu, aucune action n’est nécessaire ; sinon, ajoutez des règles valides au fichier indiqué et relancez Extract
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
    [empty_text_capture] La capture text est vide
    [rules_owner_disabled] Le fichier Rules sélectionné utilise rule = [] ; Rules a été désactivé et ses ressources extraites ont été supprimées
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
    [stdout_write_failed] Impossible d’écrire sur la sortie standard
    [stderr_write_failed] Impossible d’écrire sur la sortie d’erreur
    [stdout_flush_failed] Impossible de vider la sortie standard
    [stderr_flush_failed] Impossible de vider la sortie d’erreur
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
    [not_regular_file] La cible existante n’est pas un fichier normal
    [wrong_publisher_instance] Le jeton de publication appartient à une autre instance de publication
    [journal_corrupt] Le journal de récupération de publication est incorrect ou incomplet
    [unexpected_artifact] Un artefact inattendu du système de fichiers bloque l’opération
    [interactive_session_already_open] Une autre session SQLite interactive est déjà active
    [backup_incomplete] La sauvegarde SQLite n’a pas atteint l’état terminé
    [request_serialization_failed] La requête du modèle n’a pas pu être sérialisée
    [http_client_build_failed] Le client HTTP du service de modèles n’a pas pu être créé
    [dns_resolution_failed] La résolution DNS a échoué
    [tcp_connection_failed] La connexion TCP a échoué
    [request_send_failed] La requête HTTP n’a pas pu être envoyée
    [response_read_failed] La réponse HTTP n’a pas pu être lue
    [tls_handshake_failed] La négociation TLS a échoué
    [connect_timed_out] La connexion TCP a expiré
    [read_timed_out] La lecture de la réponse HTTP a expiré
    [request_timed_out] La requête HTTP a dépassé son délai total
    [response_decode_failed] La réponse HTTP n’a pas pu être décodée
    [redirect_rejected] La redirection HTTP a été refusée
    [response_parsing_failed] La réponse du modèle n’est pas un JSON valide
    [model_stream_invalid_json] Un événement du flux du modèle n’est pas un JSON valide
    [model_stream_invalid_utf8] Le flux du modèle contient un UTF-8 invalide
    [model_stream_error_event] Le flux du modèle a renvoyé un événement d’erreur du service
    [model_stream_unclosed_event] Un événement SSE n’a pas été fermé par une ligne vide
    [model_stream_missing_finish] Le flux Chat ne contient pas finish_reason
    [model_stream_missing_responses_terminal] Le flux Responses ne contient pas d’événement terminal
    [model_stream_event_type_mismatch] Le nom d’événement SSE et le type JSON ne correspondent pas
    [model_stream_duplicate_choice] Le flux du modèle a répété la même choice
    [model_stream_output_after_finish] Le flux du modèle a continué après finish
    [model_stream_unexpected_done] Le flux Responses a renvoyé un [DONE] inattendu
    [response_json_invalid] La réponse Assistant n'est pas un JSON valide
    [response_shape_invalid] La racine ou la structure de réponse du JSON Assistant est incorrecte
    [response_id_invalid] Un élément de réponse contient un output ID invalide
    [response_id_unexpected] La réponse contient un output ID qui n'a pas été demandé
    [response_id_duplicate] La réponse contient plusieurs fois le même output ID
    [response_id_missing] La réponse omet un output ID demandé
    [response_translation_not_array] translation doit être un tableau de chaînes
    [response_translation_item_not_string] Un élément du tableau translation n'est pas une chaîne
    [response_echo_shape_invalid] L'objet source renvoyé ne respecte pas la structure source/translation demandée
    [response_echo_source_item_not_string] Un élément du tableau source renvoyé n'est pas une chaîne
    [response_translation_blank] La traduction renvoyée est vide
    [response_translation_text_invalid] La traduction renvoyée contient un saut de ligne, un NUL ou une marque d'ordre des octets interdit
    [response_placeholder_snapshot_invalid] L'instantané Placeholder utilisé pour valider la réponse est invalide
    [response_placeholder_identity_or_count_mismatch] La traduction a modifié l'identité ou le nombre des Placeholders requis
    [response_placeholder_missing] Un token de contrôle requis manque dans la traduction
    [response_placeholder_unexpected] La traduction contient un token de contrôle inattendu
    [response_placeholder_order_mismatch] La traduction a modifié l'ordre requis des tokens de contrôle
    [response_placeholder_binding_mismatch] La traduction a modifié la liaison des Placeholders requis au texte
    [response_placeholder_boundary_mismatch] La traduction a ajouté ou supprimé une limite de Placeholder requise
    [response_placeholder_reserved_token] La traduction contient un token Placeholder réservé
    [response_placeholder_ambiguous] Un Placeholder renvoyé ne peut pas être associé sans ambiguïté à un token requis
    [response_control_token_invalid] La structure des tokens de contrôle renvoyée est invalide
    [response_text_segment_count_mismatch] La réponse a modifié le nombre requis de segments de texte
    [response_text_segment_shape_mismatch] La réponse a modifié la structure requise des segments de texte
    [response_line_count_mismatch] Le tableau translation ne contient pas le nombre d'éléments attendu
    [response_line_text_invalid] Un élément du tableau translation contient du texte qui ne peut pas être accepté
    [response_blank_line_mismatch] Le tableau translation n'a pas conservé les emplacements vides et non vides requis
    [response_source_residual] La traduction acceptée contient encore du texte source et doit être révisée
    [response_finish_requires_review] Le modèle s'est arrêté pour une raison non finale ; le résultat renvoyé doit être révisé
    [response_thinking_empty] Le champ think obligatoire est vide ou ne contient que des caractères d'espacement
    [response_no_usable_output] La réponse Assistant ne contient aucune sortie utilisable
    [response_all_outputs_rejected] Toutes les sorties de la réponse Assistant ont été rejetées
    [invalid_response_contract] La réponse du modèle ne respecte pas le contrat de réponse requis
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
diagnostic-http-status = Statut HTTP { $status }
diagnostic-http-route-direct = Connexion directe (sans proxy)
diagnostic-http-route-proxy = Via le proxy explicite { $proxy }
diagnostic-retry-after = Retry-After : { $seconds } secondes
diagnostic-provider-code = Code du fournisseur : { $code }
diagnostic-provider-type = Type du fournisseur : { $kind }
diagnostic-provider-message = Message du fournisseur : { $message }
diagnostic-json-position = ligne { $line }, colonne { $column }
diagnostic-response-item = élément de réponse { $item }
diagnostic-array-item = élément de tableau { $item }
diagnostic-token-position = position du token de contrôle { $position }
diagnostic-text-segment = segment de texte { $segment }
diagnostic-expected-actual = attendu { $expected }, reçu { $actual }
diagnostic-placeholder-rule-file = Règle Placeholder { $number } dans { $path }
diagnostic-placeholder-rule-project = Règle Placeholder { $number } du projet actuel
manual-exported = { $entries } entrées exportées vers { $path }
manual-checked = Valides { $valid }, non remplies { $unfilled }, erreurs { $errors }
manual-applied = Appliquées { $applied }, non remplies { $unfilled }, erreurs { $errors }
manual-value = { $code ->
    [invalid_source_line] l’élément source { $line } contient un saut de ligne ou NUL
    [invalid_translation_line] l’élément translation { $line } contient un saut de ligne ou NUL
    [fixed_length] la traduction fixed exige { $expected } éléments ; { $actual } fournis
    [fixed_blank_slot] l’élément { $line } de la traduction fixed doit rester vide
    [rerun_export] Relancez manual export
    [rerun_export_without_controls] Relancez manual export sans ajouter de saut de ligne ni NUL dans les éléments du tableau
    [rerun_export_then_fill] Relancez manual export, puis renseignez la traduction
    [resolve_temporary_then_rerun_export] Corrigez le chemin temporaire fixe affiché, supprimez tout objet résiduel, puis relancez manual export
    [resolve_published_backup_cleanup] Les deux exports sont appliqués ; vérifiez-les, puis supprimez le fichier backup fixe affiché
    [keep_exported_type] Conservez le type écrit par manual export
   *[other] __ATT_FALLBACK__
}
task-record-title = Tâche de traduction
task-record-final-result-heading = Résultat final
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
task-record-requested = Traductions demandées : { $requested }
task-record-accepted-written = Acceptées : { $accepted } entrées (ID : { $ids }), écrites à { $written } emplacements réels
task-record-accepted-outcome-unknown = Validées : { $accepted } entrées (ID : { $ids }) ; résultat du commit de base de données impossible à confirmer
task-record-unaccepted = Non acceptées : { $unaccepted } entrées (ID : { $ids })
task-record-task-diagnostic = Diagnostic de tâche
