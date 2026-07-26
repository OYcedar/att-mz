app-about = Traduire des jeux RPG Maker avec un état de projet réutilisable
cli-config-help = Fichier de configuration TOML strict pour cette exécution
cli-ui-language-help = Langue de l’aide, des diagnostics, de la progression, des résultats et des journaux : ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko ou vi
cli-progress-help = Mode de progression en direct : auto, plain ou off
cli-mz-about = Traduire un jeu RPG Maker MZ
cli-mv-about = Traduire un jeu RPG Maker MV
cli-init-about = Initialiser ou mettre à jour un projet de jeu nommé
cli-extract-about = Extraire le texte avec un plan owner explicite ou enregistré
cli-translate-about = Traduire le texte extrait avec un Profile explicite ou enregistré
cli-write-back-about = Réécrire les traductions validées dans le jeu
cli-project-lua-about = Exécuter une fois un programme Lua approuvé dans le contexte du projet
cli-project-name-help = Nom stable du projet
cli-init-path-help = Racine du jeu RPG Maker ; un projet existant peut réutiliser le dernier chemin réussi
cli-source-language-help = ID de langue source
cli-target-language-help = ID de langue cible
cli-dialogue-width-help = Nombre maximal de caractères pleine chasse par ligne de dialogue
cli-scrolling-width-help = Nombre maximal de caractères pleine chasse par ligne de texte défilant
cli-help-width-help = Nombre maximal de caractères pleine chasse par ligne d’aide ou de description
cli-builtin-help = Utiliser les emplacements de texte RPG Maker intégrés à ATT
cli-rules-help = Remplacer l’owner Rules par cette définition TOML ; une liste vide le désactive
cli-dialogue-rules-help = Remplacer la projection des noms de dialogue MV utilisée avec Builtin
cli-lua-help = Remplacer le programme Lua de la phase ; un fichier de zéro octet l’efface
cli-profile-help = ID du Profile de traduction ; l’omettre réutilise le dernier Profile réussi
cli-terms-help = Remplacer la ressource terminologique du projet
cli-placeholders-help = Remplacer la ressource Placeholder du projet
cli-project-lua-profile-help = Profile pour la validation manuelle Standard ; s’il est omis, le dernier Profile Translate réussi est résolu à l’ouverture de Standard
cli-project-lua-script-help = Programme Lua approuvé à exécuter une fois
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
log-label-phase-plan-standard = planification de l’écriture standard
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
notice-translate-reuse-lua = Aucune option Lua fournie ; réutilisation du dernier choix Translate Lua réussi.
notice-write-back-reuse-lua = Aucune option Lua fournie ; réutilisation du dernier programme WriteBack Lua réussi.
notice-write-back-standard-only = Aucun programme WriteBack Lua n’est configuré ; Standard seul sera exécuté.
notice-owner-disabled = L’owner { $owner } a été désactivé et retiré des futurs plans automatiques.
notice-lua-cleared = Le programme Lua { $phase } a été effacé ; il ne sera pas exécuté cette fois.
notice-no-model-request = Toutes les unités de traduction standard sont à jour ; Standard n’a envoyé aucune requête au modèle pendant cette exécution.
notice-manual-layout = { $count ->
    [one] 1 unité nécessite une vérification manuelle des sauts de ligne.
   *[other] { $count } unités nécessitent une vérification manuelle des sauts de ligne.
}
notice-log-degraded = La journalisation du projet est indisponible ou dégradée ; la commande continue et son code de sortie ne change pas.
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
progress-extract-lua = Exécution du programme Extract Lua
progress-extract-commit = Commit des ressources extraites
progress-translate-planning = Planification des tâches de traduction
progress-translate-confirmed = Tâches de traduction confirmées
progress-translate-no-work = Aucun appel au modèle nécessaire
progress-project-lua = Exécution du programme Lua du projet
progress-write-back-read-assets = Lecture des ressources validées
progress-write-back-planning = Planification de la réécriture des documents
progress-write-back-documents = Documents réécrits
progress-write-back-lua = Exécution du programme WriteBack Lua
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
result-translate-standard = Traduction standard : { $total } tâches ; { $complete } complètes, { $partial } partielles, { $unavailable } indisponibles ; { $written } emplacements écrits, { $remaining } restants
result-translate-convergence = Convergence : { $retained } conservés, { $invalidated } invalidés, { $not_applicable } non applicables, { $reused } réutilisés
result-write-back-completed = Réécriture terminée : { $project }
result-project-lua-completed = Exécution Lua du projet terminée : { $project }
result-output-directory = Répertoire de sortie : { $path }
result-write-back-standard = Réécriture standard : { $translated } unités traduites, { $original } unités source ; { $auto_wrapped } retours automatiques, { $breaks } sauts de ligne et { $indents } retraits pleine chasse ajoutés ; { $manual } mises en page manuelles
result-lua-executed = Lua : exécuté
result-lua-not-executed = Lua : non exécuté
result-cancelled = La commande a été annulée après une finalisation sûre.
result-plan-saved = Le plan d’exécution réussi a été enregistré.
result-translate-plan-sources = Le plan de cette exécution réussie a été enregistré. Source du Profile : { $profile_source } ; source Lua : { $lua_source }.
log-run-started = La commande { $command } a démarré.
log-run-succeeded = La commande { $command } s’est terminée avec succès.
log-run-failed = La commande { $command } a échoué.
log-run-outcome-unknown = La commande { $command } s’est terminée avec un résultat final inconnu ; suivez les emplacements de récupération indiqués dans l’erreur.
log-run-cancelled = La commande { $command } a été annulée.
log-performance-counters = Compteurs de performances : { $sqlite_control_attempted_total } tentatives de contrôle de transaction SQLite ; validations complètes de l’arborescence candidate démarrées { $candidate_validation_started }, terminées { $candidate_validation_completed }.
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
    [process_output] Sortie du processus
    [lua] Exécution Lua du projet
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
    [tag_value_contains_closing_delimiter] La ligne { $line } contient '>' qui fermerait la valeur de balise prématurément
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
