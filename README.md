# Locals Studio

Application macOS Apple Silicon et CLI pour configurer, télécharger et servir des modèles MLX avec [Locals](https://github.com/Generated-Computers/Locals).

Tauri 2 + React/Vite pour l’interface ; Rust pour le service persistant, la CLI et le routeur HTTP. Un processus Python Locals par profil actif. Fermer la fenêtre ne coupe ni les modèles ni les téléchargements.

## Démarrage

Prérequis de développement : macOS 14+, Apple Silicon, outils de compilation Xcode, Rust, Node/npm et [uv](https://docs.astral.sh/uv/). L’installation du runtime télécharge Python 3.12 et les dépendances MLX verrouillées.

```sh
npm ci
npm run desktop
```

Le script compile le service et le prépare comme sidecar Tauri. Le frontend seul (`npm run dev`) permet de travailler sur l’interface ; il affiche une connexion indisponible dans un navigateur ordinaire, car le bridge d’administration passe par Tauri.

Pour construire l’application et le DMG :

```sh
npm run desktop:build
```

Les artefacts sont générés dans `target/release/bundle/`. La signature de distribution et la notarisation utilisent les paramètres habituels de Tauri ; les builds locaux peuvent être signés ad hoc.

Les builds locaux utilisent une signature ad hoc par défaut. Après avoir placé l’application dans Applications, la CLI embarquée peut être ajoutée au PATH :

```sh
mkdir -p "$HOME/.local/bin"
ln -s "/Applications/Locals Studio.app/Contents/MacOS/locals-studio" "$HOME/.local/bin/locals-studio"
```

Le dossier `~/.local/bin` doit être présent dans le PATH du terminal.

## Parcours

- **Chat** : modèles servis, streaming et annulation, dossiers, conversations, paramètres de génération.
- **Model server** : sélection persistante, démarrage/arrêt global ou individuel, mémoire et CPU, allocations MLX et logs par worker.
- **My models** : découverte HF/LM Studio, import de dossiers sans copie, profils serveur et configuration avancée.
- **Discover** : recherche HF filtrée MLX, informations et variantes publiées, téléchargements persistants avec pause/reprise.

Les profils conservent les défauts Locals tant qu’une option n’est pas renseignée. Une modification de profil s’applique au prochain démarrage du worker. Les champs `host`, `port` et `model` des workers sont réservés à Studio.

## CLI

```sh
cargo build --locked -p studio-cli
./target/debug/locals-studio runtime install
./target/debug/locals-studio models scan
./target/debug/locals-studio config profiles
./target/debug/locals-studio server start NOM_DU_PROFIL
./target/debug/locals-studio server status --json
./target/debug/locals-studio server logs NOM_DU_PROFIL --follow
./target/debug/locals-studio server stop --all
```

Le service démarre automatiquement au premier appel. `locals-studio daemon` le lance au premier plan. `locals-studio events` suit les événements JSON Lines. Les commandes de consultation acceptent `--json` ; les erreurs arrivent sur stderr avec un code de sortie non nul.

```sh
locals-studio search "Qwen MLX 4bit"
locals-studio download add ORGANISATION/DEPOT --revision main
locals-studio download list
locals-studio download pause IDENTIFIANT
locals-studio download resume IDENTIFIANT
locals-studio models import /chemin/vers/un/modele
locals-studio config set selected true --profile NOM_DU_PROFIL
locals-studio config set options.max_context 8192 --profile NOM_DU_PROFIL
locals-studio config set options.cache_gb 1 --profile NOM_DU_PROFIL
locals-studio server start --all
```

Si l’estimation mémoire demande une confirmation, ajuster les profils ou passer explicitement `--allow-memory-overcommit`. Cette décision vaut pour le lancement demandé et n’active pas de déchargement automatique.

## API commune et LAN

Par défaut : `http://127.0.0.1:8321/v1`. Studio agrège les modèles prêts et route chaque requête selon le champ `model`, qui contient l’alias du profil.

Pour tester un modèle déjà démarré :

```sh
curl --fail-with-body -sS http://127.0.0.1:8321/health
curl --fail-with-body -sS http://127.0.0.1:8321/v1/models

# Remplacer NOM_DU_MODELE par un id renvoyé par /v1/models.
curl --fail-with-body -N http://127.0.0.1:8321/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"NOM_DU_MODELE","messages":[{"role":"user","content":"Réponds uniquement OK."}],"max_tokens":16,"temperature":0,"stream":true}'
```

`-N` affiche les événements SSE au fil de leur arrivée. Pour recevoir une réponse JSON complète, passer `stream` à `false`. `Ctrl-C` coupe le flux et annule la requête en cours. Une liste de modèles vide signifie qu’aucun worker n’est prêt ; le démarrer depuis Model Server ou `locals-studio server start ALIAS`.

```sh
locals-studio config set api.host 0.0.0.0
locals-studio config set api.port 8321
```

Depuis une autre machine : `http://IP_DU_MAC:8321/v1`. Les workers restent liés à `127.0.0.1` sur leurs ports internes. L’administration, les historiques, le runtime et les téléchargements restent accessibles uniquement par le socket Unix local.

Les routes d’inférence OpenAI, Anthropic et Ollama sont relayées. Le streaming reste progressif ; fermer la connexion client annule la connexion amont. La gestion des téléchargements utilise le service Studio, pas le faux endpoint de téléchargement Ollama de Locals.

Les listes `api.allowed_hosts` et `api.allowed_origins` sont facultatives et peuvent rester vides. Les hôtes locaux et LAN détectés automatiquement ainsi que les origines navigateur locales restent autorisés par défaut. Ajouter les noms personnalisés avec `api.allowed_hosts`. Un navigateur distant nécessite une origine exacte dans `api.allowed_origins` ; les workers démarrés ensuite reçoivent cette liste. Après modification des origines, redémarrer les workers concernés. Les contrôles Host/Origin ne constituent pas une authentification des clients LAN.

```sh
locals-studio config set api.allowed_hosts '["mon-mac.local"]'
locals-studio config set api.allowed_origins '["http://192.168.1.20:3000"]'
```

## Stockage, runtime et mémoire

- Données d’administration : dossier utilisateur `Locals Studio` dans Application Support sur macOS ; base SQLite et logs locaux.
- Nouveaux modèles : `HF_HOME/locals-studio`, par défaut `~/.cache/huggingface/locals-studio`.
- Les bibliothèques HF et LM Studio sont référencées sans copie. Retirer une inscription conserve les poids.
- Les téléchargements fixent un commit HF, vérifient les empreintes disponibles, checkpointent des segments de 64 Mio et publient le dossier complet par renommage atomique. Un arrêt brutal peut nécessiter de reprendre le segment courant. Annuler supprime le staging du job ; mettre en pause le conserve.
- Le transport HTTP vérifie les plages retournées. Si le serveur ou un proxy renvoie le fichier entier, la reprise conserve les segments sur disque mais relit et compare le préfixe sur le réseau avant de continuer. Ce cas peut donc retransférer des octets déjà reçus. Les certificats sont validés avec la confiance du système.
- Les identifiants HF ambiants (`HF_TOKEN`, fichier de jeton HF) servent aux dépôts privés/gated ; les licences doivent être acceptées sur HF.
- Le runtime utilise le checkout Locals voisin en développement ou l’archive officielle épinglée dans `resources/runtime/source.lock.json` en distribution. Les dépendances sont verrouillées avec hashes ; les mises à jour sont explicites.
- RAM courante et CPU viennent du système ; les allocations GPU actives/cache/pic viennent de MLX. Le RSS fourni par Locals est un pic. Les vues RSS/MLX peuvent se recouvrir en mémoire unifiée et ne sont pas additionnées. Le pourcentage GPU n’est pas inventé quand aucun compteur fiable n’est disponible.
- L’estimation avant chargement est partielle pour les architectures inconnues ou les réglages Auto. Elle compare les poids et enveloppes estimées au budget Metal, avec réserve système, sans garantir une limite totale du processus.

Variables de développement : `LOCALS_STUDIO_DATA_DIR`, `LOCALS_STUDIO_MODELS_DIR`, `LOCALS_STUDIO_LOCALS_SOURCE`, `LOCALS_STUDIO_CLI_BIN`. Elles permettent aussi d’isoler complètement les tests des données personnelles.

## Validation

```sh
cargo test --locked -p studio-core -- --test-threads=1
npm test
npm run build
```

Les tests du cœur utilisent de vrais petits serveurs HTTP et des processus Python factices, sans télécharger ni charger de LLM. Ils couvrent plages de téléchargement, checkpoints, intégrité, découverte, propriété des processus, démarrage/arrêt, routage, streaming et politiques réseau. La validation MLX réelle et l’essai depuis une seconde machine du LAN sont distincts ; voir `docs/validation.md` pour les résultats effectivement obtenus.

Licence MIT, voir [LICENSE](LICENSE). Locals est un projet séparé sous Apache-2.0 ; son code n’est pas modifié par Studio.
