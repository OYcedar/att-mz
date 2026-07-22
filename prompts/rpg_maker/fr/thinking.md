# Exigences relatives à la sortie de réflexion

Pour l'ensemble du TaskBlock, produisez exactement un bloc `<why>...</why>` avant le JSON final.

- La réponse doit commencer immédiatement par la balise exacte `<why>`, en minuscules et sans attribut. Ne produisez aucun texte introductif avant elle et n'imbriquez ni ne répétez `<why>`.
- Le contenu de `<why>` doit rester non vide après Unicode `trim()` et analyser réellement chaque entrée marquée par `[ID]` :
  1. le locuteur, le destinataire, le sujet omis et la personne grammaticale possible ;
  2. les relations entre personnages, le ton, l'émotion et le niveau de politesse ;
  3. le sens des termes et leur expression naturelle dans la langue cible ;
  4. les espaces réservés, les codes de contrôle, chaque ATT token et la structure de lignes imposée par `single line`, `free line breaking`, `N lines, corresponding line by line` ou `N items, corresponding item by item` ;
  5. les valeurs `[ID]`, le nombre de lignes, les résidus de la langue source et le format final.
- Ne vous contentez pas d'écrire « vérifié » et ne passez pas directement à la conclusion ; fournissez une analyse concrète. Aucun titre de rubrique fixe n'est imposé. ATT vérifie seulement que le contenu de réflexion n'est pas vide et ne juge pas si l'analyse est correcte.
- Terminez ce bloc unique par la balise exacte `</why>`, en minuscules et sans attribut. Seuls des espaces peuvent séparer `</why>` du JSON ; produisez ensuite directement le JSON exigé par le system Prompt. Le JSON ne doit pas se trouver dans `<why>` et aucun second bloc `<why>...</why>` n'est autorisé.
