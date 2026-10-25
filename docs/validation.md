# Validation de Locals Studio 0.1.0

Validation locale du 25 octobre 2026, macOS Apple Silicon, 24 Go de mémoire unifiée. Les essais avec des workers factices, les essais MLX réels et les contrôles du paquet installable sont distingués ci-dessous.

## Tests automatisés et construction

| Vérification | Résultat | Couverture |
| --- | --- | --- |
| Cœur Rust | **44 tests réussis : 38 unitaires et 6 d’intégration.** | HTTP et processus Python factices, sans chargement de LLM. |
| Téléchargements | Réussi | Révisions et manifestes épinglés, checkpoints de 64 Mio, pause/reprise/annulation, récupération après interruption, réponses 206, réponse 200 avec relecture du préfixe, tailles, SHA-256 et empreintes Git, refus des fichiers corrompus. |
| Bibliothèques et persistance | Réussi | HF et LM Studio, dossiers personnalisés, références sans copie, conservation des poids après désinscription, SQLite. |
| Supervision | Réussi | Ports distincts, propriété des processus, réconciliation, arrêt pendant chargement, requêtes actives, logs bornés, compteurs indisponibles et stabilité des mesures CPU. |
| Profils supprimés | Réussi | Absence de recréation après scan ou redémarrage, nettoyage des anciens workers arrêtés et réutilisation d’un alias sans intercepter le nouveau worker prêt. |
| API commune | Réussi | Deux workers, alias exacts dont `-thinking`, streaming progressif, annulation par déconnexion, Host/Origin, préflights et conflit de port public. |
| Chat | Réussi | Annulation et conservation de la réponse partielle ; suppression d’un dossier pendant un stream sans perdre la réponse. |
| Frontend | **15 tests réussis : 7 helpers et 8 composants React.** | Brouillons, navigation, événements, fin de stream, paramètres automatiques et booléens, logs, redimensionnement, dossiers, arrêt pendant démarrage et conservation du contenu pendant les rafraîchissements CPU/RAM, logs et estimation mémoire ; enregistrement des listes réseau facultatives vides. Transport Tauri simulé uniquement dans les tests. |
| Construction | Réussi | TypeScript, Vite, format Rust et build Tauri release avec CLI embarquée. |

```sh
cargo test --locked -p studio-core -- --test-threads=1
cargo fmt --all -- --check
npm test
npm run desktop:build
```

Les compilations ont été séparées des chargements MLX gérés pour ne pas fausser leur budget mémoire.

## Runtime, téléchargement et inférence réels

- Runtime installé via `uv` : Python **3.12**, Locals **0.2.1**, dépendances verrouillées avec empreintes. Source de distribution épinglée au commit `4f4d03f10232c9f2adeb1e96e2a7bd79a9e7fe19`.
- Téléchargement complet de **mlx-community/Qwen3.5-0.8B-MLX-4bit**, commit `5d894f8cc4ef3e6c88537bf3746ed262f549da6a` : **652 027 143 octets**, poids vérifiés par SHA-256 avant publication. Transport HTTP continu, `replayed_bytes=0` : la reprise réseau réelle n’est pas revendiquée pour cet essai ; elle est testée automatiquement.
- Inférence **Qwen3.5 4B** via le port Studio **8321** : HTTP 200, réponse **OK**, 18 tokens de prompt et 1 de réponse. Paramètres bornés : contexte 512, sortie 16, cache 0,125 Go, 1 slot. Worker arrêté après l’essai.
- Inférence **0,8B dans la GUI** : démarrage depuis Model Server, réponse **OK**, 18 tokens de prompt et 1 de réponse. Un second essai du paquet final, contexte 512 et sortie explicitement bornée à **64 tokens**, a été interrompu avec le bouton Stop : réponse partielle conservée, `cancelled=true`, état **Stopped** et journal worker confirmant la déconnexion client.
- Fermeture native du paquet final : **PID GUI absent**, modèle toujours prêt sur **8321**, liste HTTP et contrôle CLI opérationnels. Après réouverture, conversation partielle, dossier, sélection et configuration du profil retrouvés.
- Thèmes clair/sombre, profils, logs, MLX actif/cache/pic, pic RSS Locals et estimation de mémoire libérable examinés dans l’application native. RSS et allocations MLX ne sont pas additionnés.

Le serveur Locals indépendant déjà présent sur **8322** a été conservé : PID inchangé et santé vérifiée. Les profils, conversations et workers créés uniquement pour ces essais ont été nettoyés ; les modèles téléchargés restent disponibles.

## API depuis une seconde machine du LAN

**12 contrôles sur 12 réussis** depuis un autre Mac, joint en SSH, vers l’adresse LAN du Mac hébergeant Studio sur **8321**.

| Contrôle | Résultat |
| --- | --- |
| Modèles | HTTP 200 avant et après redémarrage du worker. |
| Génération SSE sans Origin | HTTP 200, contenu puis `[DONE]`, sortie bornée à 8 tokens. |
| Host | Nom configuré accepté ; hôte inconnu refusé avec 403. |
| Origin | Préflight autorisé 204 ; POST et préflight d’une origine inconnue refusés avec 403. |
| Nouveau navigateur autorisé | Worker déjà actif : 403 attendu. Après redémarrage explicite : SSE 200 avec origine autorisée et en-tête Authorization. Cet en-tête ne démontre pas une authentification LAN. |
| Isolation | Les deux ports internes utilisés successivement par le worker sont inaccessibles depuis l’autre Mac. |
| Restauration | Configuration originale rétablie exactement sur **127.0.0.1:8321**, workers de test arrêtés, serveur indépendant préservé. |

Il s’agit de requêtes HTTP distantes avec les en-têtes indiqués, pas d’un essai interactif dans un navigateur distant.

## Artefacts et limites

| Artefact | Résultat |
| --- | --- |
| `target/release/bundle/macos/Locals Studio.app` | **21,03 Mio**, frontend embarqué `tauri://localhost`, Vite arrêté. |
| `target/release/bundle/dmg/Locals Studio_0.1.0_aarch64.dmg` | **7,38 Mio**, intégrité `hdiutil verify` validée. |
| Signature | **Ad hoc**, `codesign --verify --deep --strict` réussi. Non notarisé Apple. |

Le runtime Python et les poids sont installés séparément et ne sont pas compris dans ces tailles. La révision source livrée est le commit Git contenant cette fiche ; les artefacts sont générés localement et ne sont pas versionnés.

Les preuves locales non versionnées sont conservées sous `target/qa/` : `real-inference.json`, `lan.json`, `final-native.json` et captures de la GUI. Aucun test de charge prolongé, de deux LLM MLX chargés simultanément par Studio, de téléchargement réel actif pendant la fermeture de la GUI ou de reprise réelle après coupure réseau n’est revendiqué. Les scénarios de reprise et de concurrence sont couverts par les tests automatisés. L’estimation mémoire reste partielle pour les architectures hybrides et les réglages Auto ; elle ne constitue pas une limite stricte du processus.
