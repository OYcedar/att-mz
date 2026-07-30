app-about = Traduire des jeux et du texte structuré avec un état de projet réutilisable
cli-config-help = Fichier de configuration TOML strict pour cette exécution
cli-ui-language-help = Langue de l’aide, des diagnostics, de la progression, des résultats et des journaux : ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko ou vi
cli-progress-help = Mode de progression en direct : auto, plain ou off
cli-mz-about = Traduire un jeu RPG Maker MZ
cli-mv-about = Traduire un jeu RPG Maker MV
cli-generic-about = Traduire du texte JSONL structuré
cli-init-about = Initialiser ou mettre à jour un projet de traduction nommé
cli-extract-about = Synchroniser le texte source depuis l’entrée actuelle du projet
cli-translate-about = Traduire le texte extrait avec un Profile explicite ou enregistré
cli-write-back-about = Écrire les traductions actuelles dans la sortie du projet
cli-project-lua-about = Exécuter une fois un Lua atomique de base de données dans le projet
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
cli-project-lua-script-help = Programme Lua atomique de base de données à exécuter une fois
cli-project-lua-arguments-help = Argument UTF-8 transmis à Lua arg[1..] après --
cli-usage-heading = Utilisation :
cli-commands-heading = Commandes :
cli-options-heading = Options :
cli-arguments-heading = Arguments :
cli-options-metavar = OPTIONS
cli-command-metavar = COMMANDE
cli-print-help = Afficher l’aide
cli-print-version = Afficher la version
cli-missing-config = Le chemin de configuration requis --config <FILE> est absent.
cli-blank-value = La valeur ne peut pas être vide.
cli-invalid-positive-integer = La valeur doit être un entier positif.
cli-invalid-progress = Le mode de progression { $value } n’est pas pris en charge ; utilisez auto, plain ou off.
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
log-label-phase-check-project = vérification du projet
log-label-phase-scan-source = analyse de la source
log-label-phase-prepare-candidate = préparation du candidat
log-label-phase-update-database = mise à jour de la base de données
log-label-phase-publish = publication
log-label-phase-builtin = extraction intégrée
log-label-phase-rules = extraction par règles
log-label-phase-lua = traitement Lua
log-label-phase-planning = planification
log-label-phase-confirmed-tasks = confirmation des tâches
log-label-phase-no-work = aucun travail requis
log-label-phase-read-assets = lecture des ressources
log-label-phase-plan-rpg-maker-write-back = planification de la réécriture RPG Maker
log-label-phase-rewrite-documents = réécriture des documents
log-label-phase-validate-candidate = validation du candidat
log-label-task-complete = complet
log-label-task-partial = partiel
log-label-task-unavailable = indisponible
log-label-task-failed = échec
error-state-applied-finalization = Le résultat a pris effet, mais la finalisation a échoué. Vérifiez l’état du projet avant de réessayer.
error-no-executable-extract-owner = Après l’effacement, aucun owner Extract n’est exécutable ; le plan n’a donc pas été enregistré.
error-plan-save-failed-applied = Le résultat a pris effet, mais le nouveau plan d’exécution n’a pas été enregistré. Indiquez explicitement les options voulues la prochaine fois.
error-plan-save-outcome-unknown = Le résultat a pris effet, mais le commit du plan ne peut pas être confirmé. Indiquez explicitement les options voulues la prochaine fois.
plan-source-explicit = entrée explicite
plan-source-project-state = état du projet
plan-source-product-default = comportement du produit
notice-init-reuse-path = Aucun chemin source fourni ; réutilisation du dernier chemin réussi : { $path }.
notice-extract-reuse-owners = Aucune portée d’extraction fournie ; réutilisation du dernier plan réussi : { $owners }.
notice-translate-reuse-profile = Aucun Profile fourni ; réutilisation du dernier Profile réussi : { $profile }.
notice-owner-disabled = L’owner { $owner } a été désactivé et retiré des futurs plans automatiques.
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
result-cancelled = La commande a été annulée après une finalisation sûre.
result-plan-saved = Le plan d’exécution réussi a été enregistré.
log-run-started = La commande { $command } a démarré.
log-run-succeeded = La commande { $command } s’est terminée avec succès.
log-run-failed = La commande { $command } a échoué.
log-run-outcome-unknown = La commande { $command } s’est terminée avec un résultat final inconnu ; suivez les emplacements de récupération indiqués dans l’erreur.
log-run-cancelled = La commande { $command } a été annulée.
log-performance-counters = Compteurs de performances : { $sqlite_control_attempted_total } tentatives de contrôle de transaction SQLite ; validations complètes de l’arborescence candidate démarrées { $candidate_validation_started }, terminées { $candidate_validation_completed }.
log-lua-script = Script Lua { $identity } (SHA-256 { $fingerprint }).
log-lua-print = Lua : { $message }
log-lua-summary = Lua validé : { $database_calls } appels à la base, { $changed_rows } lignes modifiées, { $translation_calls } appels de traduction et { $printed_lines } lignes affichées.
log-plan-resolved = Le plan de { $command } provient de { $source }.
log-phase-started = Phase démarrée : { $phase }.
log-phase-finished = Phase terminée : { $phase }.
log-retry-summary = { $count ->
    [one] 1 nouvelle tentative a été effectuée.
   *[other] { $count } nouvelles tentatives ont été effectuées.
}
log-no-work = Aucun travail requis : { $reason }.
log-no-work-translation-up-to-date = les traductions correspondent déjà à la source et au profil actuels
log-partial-result = { $count ->
    [one] 1 résultat partiel nécessite une attention.
   *[other] { $count } résultats partiels nécessitent une attention.
}
log-translation-task-started = Tâche de traduction { $index }/{ $total } démarrée.
log-translation-task-finished = Tâche de traduction { $index } terminée avec le résultat { $outcome }.
log-translation-task-diagnostic = La tâche de traduction { $index } a signalé un diagnostic après { $attempts } tentatives : { $diagnostic }
diagnostic-title = Erreur [{ $code }]
diagnostic-stage = Étape : { $stage }
diagnostic-subject = Emplacement : { $subject }
diagnostic-subject-value = { $kind ->
    [command] commande { $value }
    [field] champ { $value }
    [project] projet { $value }
    [profile] profil { $value }
    [component] composant { $value }
   *[other] { $value }
}
diagnostic-reason = Cause : { $reason }
diagnostic-impact = Impact : { $impact }
diagnostic-action = Action : { $action }
diagnostic-recovery = Récupération : { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] composant { $value }
    [transaction] transaction { $value }
   *[other] { $value }
}
diagnostic-related = Erreur associée { $index } :
diagnostic-stage-value = { $code ->
    [process_startup] Démarrage du processus
    [process_output] Sortie du processus
    [configuration] Chargement de la configuration
    [command_preparation] Préparation de la commande
    [project_opening] Ouverture du projet
    [init] Initialisation
    [extract] Extraction
    [translate] Traduction
    [write_back] Réécriture
    [lua] Exécution Lua du projet
    [model_request] Requête au modèle
    [run_plan_finalization] Finalisation du plan d’exécution
    [publication] Publication
    [shutdown] Arrêt
    [logging] Journal du projet
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] L’état n’a pas été modifié
    [valid_progress_preserved] La progression valide a été conservée
    [result_applied_but_run_plan_not_saved] Le résultat a été appliqué, mais le plan d’exécution n’a pas été enregistré
    [state_applied_but_finalization_failed] L’état a été appliqué, mais la finalisation n’a pas abouti
    [recovery_required] Une récupération est requise avant de pouvoir faire confiance à l’état
    [outcome_unknown] L’état final est inconnu
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] Corrigez le champ de configuration indiqué, puis réessayez
    [fix_input] Corrigez l’entrée indiquée, puis réessayez
    [check_path_and_permissions] Vérifiez le chemin, l’état du système de fichiers et les autorisations
    [check_project_state] Examinez et corrigez l’état du projet, puis réessayez
    [retry_after_resolving_contention] Attendez la fin de l’opération concurrente, puis réessayez
    [check_model_service] Vérifiez la réponse du service de modèle et les limites du compte
    [preserve_recovery_artifacts] Ne supprimez pas les artefacts de récupération indiqués ; récupérez la sortie avant de réessayer
    [retry] Réessayez l’opération
    [report_bug] Signalez ce défaut ATT avec le code d’erreur et le chemin du journal
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Une valeur obligatoire est manquante
    [extract_plan_required] Aucun plan Extract réutilisable n’est enregistré ; fournissez --builtin ou --rules
    [generic_extract_required] L’entrée JSONL ne correspond plus au dernier Extract ; exécutez de nouveau att generic extract
    [conflicting_values] Les valeurs fournies sont incompatibles
    [invalid_syntax] La syntaxe de la valeur est incorrecte
    [invalid_encoding] L’encodage du texte est incorrect
    [invalid_value] La valeur ne respecte pas le contrat requis
    [not_found] L’objet requis n’existe pas
    [busy] La ressource est utilisée par une autre opération
    [state_mismatch] L’état enregistré du projet ne satisfait pas cette opération
    [requirement_failed] Une condition préalable requise n’est pas satisfaite
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
    [lua_database_open_failed] L’hôte Lua n’a pas pu ouvrir la session de base de données du projet
    [lua_context_creation_failed] L’environnement Lua n’a pas pu créer le contexte VM
    [lua_compilation_failed] Le programme Lua principal n’a pas pu être compilé
    [lua_execution_failed] Le programme Lua principal a échoué pendant son exécution
    [lua_host_call_failed] Un appel à une capacité de l’hôte Lua a échoué
    [lua_finalization_failed] L’hôte Lua n’a pas pu finaliser toutes les ressources liées
    [rules_definition_invalid] Le programme Rules ne respecte pas le contrat de définition Rules
    [rules_document_read_failed] Un document source requis par le programme Rules n’a pas pu être lu
    [rules_no_non_blank_match] L’entrée Rules n’a produit aucune unité sémantique non vide
    [rules_invalid_target] L’entrée Rules a sélectionné une valeur inutilisable comme cible de texte
    [rules_pattern_match_failed] Le motif PCRE2 de Rules n’a pas pu être évalué
    [rules_zero_width_match] Le motif Rules a produit une correspondance de largeur nulle
    [rules_overlapping_capture] Le motif Rules a produit des captures de texte qui se chevauchent
    [rules_missing_text_capture] La capture de texte nommée requise n’a pas participé à la correspondance
    [rules_invalid_capture_range] La correspondance ou la capture Rules est hors des limites de caractères UTF-8 valides
    [rules_duplicate_target] Deux entrées Rules revendiquent la même cible de texte physique
    [rules_invalid_materialization] La recette de projection Rules ne peut pas reconstruire la valeur source
    [rules_snapshot_invalid] Les groupes Rules extraits ne forment pas un instantané de ressources valide
    [rules_snapshot_store_failed] L’instantané d’extraction Rules validé n’a pas pu être enregistré
    [write_back_extraction_out_of_date] Les ressources extraites ne correspondent plus à la source actuelle du projet
    [write_back_asset_snapshot_invalid] Les ressources RPG Maker enregistrées ne forment pas un instantané de réécriture valide
    [source_document_invalid] Un document source RPG Maker ne respecte pas le format requis
    [write_back_mutation_invalid] Une modification de traduction validée ne peut pas être appliquée à son emplacement source figé
    [write_back_output_path_invalid] Un fichier réécrit se trouve hors de l’arborescence de sortie RPG Maker autorisée
    [write_back_output_path_duplicate] Plusieurs fichiers réécrits ciblent le même chemin de sortie
    [write_back_candidate_project_mismatch] Le candidat de réécriture préparé appartient à un autre projet
    [write_back_candidate_invalid] Le candidat de réécriture ne respecte pas l’arborescence data/js requise
    [write_back_not_published] Le candidat de réécriture n’a pas remplacé le répertoire de sortie actuel
    [write_back_published_with_residuals] La sortie a été publiée, mais certains artefacts de récupération n’ont pas pu être supprimés
    [write_back_recovery_required] Le répertoire de sortie doit être récupéré avant que son contenu soit fiable
    [internal_invariant] Un invariant interne a été violé ; il s’agit d’un défaut ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] Introuvable
    [permission_denied] Autorisation refusée
    [connection_refused] Connexion refusée
    [connection_reset] Connexion réinitialisée
    [host_unreachable] Hôte inaccessible
    [network_unreachable] Réseau inaccessible
    [connection_aborted] Connexion interrompue
    [not_connected] Non connecté
    [address_in_use] Adresse déjà utilisée
    [address_not_available] Adresse indisponible
    [network_down] Réseau hors service
    [broken_pipe] Canal rompu
    [already_exists] Existe déjà
    [would_block] L’opération serait bloquante
    [not_a_directory] N’est pas un répertoire
    [is_a_directory] Est un répertoire
    [directory_not_empty] Répertoire non vide
    [read_only_filesystem] Système de fichiers en lecture seule
    [stale_network_file_handle] Descripteur de fichier réseau obsolète
    [invalid_input] Entrée d’opération incorrecte
    [invalid_data] Données incorrectes
    [timed_out] Délai de l’opération dépassé
    [write_zero] L’écriture n’a pas progressé
    [storage_full] Stockage plein
    [not_seekable] L’objet ne permet pas le positionnement
    [quota_exceeded] Quota de stockage dépassé
    [file_too_large] Fichier trop volumineux pour le système sous-jacent
    [resource_busy] Ressource occupée
    [executable_file_busy] Fichier exécutable occupé
    [deadlock] L’opération provoquerait un interblocage
    [crosses_devices] L’opération traverse plusieurs périphériques de système de fichiers
    [too_many_links] Trop de liens de système de fichiers
    [invalid_filename] Nom de fichier incorrect
    [argument_list_too_long] Liste d’arguments du système trop longue
    [interrupted] Opération interrompue
    [unsupported] Opération non prise en charge
    [unexpected_eof] Fin de fichier inattendue
    [out_of_memory] Le système n’a pas pu allouer de mémoire
    [other] Autre erreur du système d’exploitation
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [runtime_configuration_invalid] La configuration d’exécution est incorrecte
    [unsupported_prompt_locale] La valeur doit être exactement auto en minuscules ou une langue d’interface BCP 47 prise en charge
    [language_policy_term_blank] Le terme de politique linguistique ne doit pas être vide
    [language_policy_term_surrounding_whitespace] Le terme de politique linguistique ne doit pas contenir d’espaces en début ou fin
    [language_policy_term_duplicate] Le terme de politique linguistique ne doit pas être dupliqué
    [quote_repair_candidates_empty] La liste des candidats de réparation des guillemets ne doit pas être vide
    [quote_repair_delimiter_invalid] Le délimiteur de réparation des guillemets ne doit être ni alphanumérique, ni un espace, ni un caractère de contrôle
    [quote_repair_pair_duplicate] La paire de réparation des guillemets ne doit pas être dupliquée
    [quote_repair_delimiter_ambiguous] Le délimiteur de réparation des guillemets doit appartenir à une seule paire
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
diagnostic-io-reason = Opération { $operation } : { $kind }
diagnostic-io-reason-with-os-code = Opération { $operation } : { $kind } (OS { $os_code })
diagnostic-io-reason-with-system-message = Opération { $operation } : { $kind } : { $system_message }
diagnostic-io-reason-with-os-code-and-system-message = Opération { $operation } : { $kind } (OS { $os_code }) : { $system_message }
diagnostic-failure-with-detail = { $failure } : { $detail }
diagnostic-invalid-utf8 = UTF-8 incorrect à l’octet { $valid_up_to }, longueur incorrecte de { $error_len } octets
diagnostic-incomplete-utf8 = Séquence UTF-8 incomplète après l’octet { $valid_up_to }
diagnostic-toml-failure-value = { $code ->
    [syntax] La syntaxe TOML est incorrecte
    [missing_field] Un champ de configuration obligatoire est manquant
    [unknown_field] La configuration contient un champ inconnu
    [duplicate_field] Le champ de configuration est déclaré plusieurs fois
    [type_mismatch] Type attendu : { $expected }
    [invalid_value] La valeur de configuration ne respecte pas le contrat du champ
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] une chaîne
    [integer] un entier
    [boolean] un booléen
    [string_or_boolean] une chaîne ou un booléen
    [string_array] un tableau de chaînes
    [integer_array] un tableau d’entiers
    [string_pair_array] un tableau de paires de chaînes
    [table] une table
    [table_array] un tableau de tables
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = TOML incorrect ({ $resource }) : { $failure }
diagnostic-invalid-toml-at = TOML incorrect à la ligne { $line }, colonne { $column } ({ $resource }) : { $failure }
diagnostic-http-no-details = La requête au service de modèle a échoué sans détail public sur l’état HTTP
diagnostic-http-status = État HTTP { $status }
diagnostic-http-retry-after = Retry-After de { $seconds } secondes
diagnostic-http-provider-code = Code d’erreur du fournisseur { $code }
diagnostic-http-provider-type = Type d’erreur du fournisseur { $kind }
diagnostic-http-fact-separator = ;{ " " }
diagnostic-sqlite = Code d’erreur SQLite principal { $primary_code }, code étendu { $extended_code }
diagnostic-windows-status = L’opération Windows { $operation } a échoué avec NTSTATUS { $status }
diagnostic-resource = { $resource } : valeur réelle { $actual }
diagnostic-resource-with-maximum = { $resource } : valeur réelle { $actual }, maximum { $maximum }
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
    [json] JSON de réponse du modèle invalide (catégorie `{ $category }`), ligne { $line }, colonne { $column }
    [thinking_not_allowed] la sortie de raisonnement n’est pas acceptée dans ce mode de réponse, ligne { $line }, colonne { $column }
    [thinking_envelope_missing] l’enveloppe de raisonnement requise est absente, ligne { $line }, colonne { $column }
    [thinking_envelope_unclosed] l’enveloppe de raisonnement n’est pas fermée, ligne { $line }, colonne { $column }
    [thinking_empty] le contenu du raisonnement est vide, ligne { $line }, colonne { $column }
    [thinking_nested] une enveloppe de raisonnement imbriquée commence ligne { $line }, colonne { $column }
    [thinking_repeated] une enveloppe de raisonnement répétée commence ligne { $line }, colonne { $column }
    [markdown_fence_no_body] la clôture Markdown n’a pas de contenu, ligne { $line }, colonne { $column }
    [markdown_fence_unsupported] seule une clôture Markdown unique sans balise de langue ou avec la balise json est acceptée, ligne { $line }, colonne { $column }
    [markdown_fence_unclosed] la clôture Markdown n’est pas fermée, ligne { $line }, colonne { $column }
   *[markdown_fence_invalid_closing] la clôture Markdown doit se fermer sur la dernière ligne isolée, ligne { $line }, colonne { $column }
}
task-record-attempt-succeeded = Tentative { $number } : réussie ; finish reason { $finish_reason }
task-record-attempt-token-usage = ; tokens `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; durée `{ $duration }`
task-record-attempt-request-id = ; request ID { $request_id }
task-record-attempt-response-id = ; response ID { $response_id }
task-record-attempt-retryable = Tentative { $number } : échec réessayable ; diagnostic `{ $code }` ; durée `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; nouvelle tentative après `{ $duration }`
task-record-attempt-wait-completed = ; attente de `{ $duration }` terminée ; tentative suivante non démarrée
task-record-attempt-wait-cancelled = ; attente prévue de `{ $duration }` ; annulation pendant l’attente
task-record-attempt-failed = Tentative { $number } : échec de traitement de la requête ou réponse ; diagnostic `{ $code }` ; durée `{ $duration }`
task-record-attempt-cancelled = Tentative { $number } : annulée ; durée `{ $duration }`
task-record-structured-reason = Motif : { $reason }
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
task-record-rejected-heading = Non acceptées :
task-record-rejected-item = { $id } : { $reason }
task-record-protocol-diagnostic = Diagnostic de protocole : { $diagnostic }
task-record-unavailable-reason = Motif d’indisponibilité : { $reason }
task-record-task-diagnostic = Diagnostic de tâche : `{ $code }` ; motif { $reason }
task-record-rejection-reason = { $code ->
    [missing] Sortie du modèle manquante
    [duplicate] Sortie du modèle en double
    [invalid_shape] { $detail }
    [invalid_shape_array] La traduction doit être un tableau de chaînes
    [invalid_shape_item] L’élément { $line } du tableau de traduction doit être une chaîne
    [line_count_mismatch] Nombre de lignes différent (attendu { $expected }, obtenu { $actual })
    [invalid_line_text] La ligne { $line } contient des caractères de contrôle invalides
    [blank_line_mismatch] État vide différent à la ligne { $line } (attendu : { $expected_blank ->
        [blank] vide
       *[other] non vide
    })
    [blank_translation] La traduction est vide
    [no_natural_language_text] La traduction ne contient aucun texte en langue naturelle
    [contains_byte_order_mark] La traduction contient un BOM
    [placeholder_mismatch] Placeholder différent : { $detail }
    [unexpected_placeholder] Placeholder inattendu : { $detail }
    [placeholder_normalization_ambiguous] Normalisation du placeholder ambiguë : { $detail }
    [source_residual] Résidu de la langue source détecté : { $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason n’est pas stop : { $detail }
    [invalid_response] { $detail }
    [invalid_id] L’entrée { $index } du modèle possède un ID invalide
    [unknown_id] L’entrée { $index } du modèle a renvoyé l’ID inconnu { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] Impossible d’analyser la réponse du modèle
    [all_outputs_rejected] Toutes les sorties du modèle ont été rejetées
    [recoverable_request_exhausted] Budget de nouvelles tentatives récupérables épuisé
    [retry_after_exceeds_maximum] Retry-After dépasse l’attente maximale configurée
   *[other] { $code }
}
task-record-duration-seconds = { $value } secondes
task-record-duration-milliseconds = { $value } ms
