# mana — Design v1

## Vue d'ensemble

`mana` est un CLI/TUI en Rust qui orchestre des agents IA de coding CLI existants (Claude Code en v1, autres CLIs listés mais non garantis). Il ne code jamais lui-même : un agent CLI est lancé en rôle **Main Agent (PM)**, qui discute avec l'utilisateur, découpe le travail en tâches, et lance des sous-agents **executor**/**reviewer** pour les réaliser et les relire. Toute la mécanique d'orchestration (monitoring, logging, notifications) est gérée par le binaire `mana`, jamais par les agents eux-mêmes.

Cross-platform (macOS, Linux, Windows).

## Stack

| Crate | Rôle |
|---|---|
| `clap` | CLI parsing |
| `ratatui` + `crossterm` | TUI |
| `portable-pty` | Spawn cross-platform des CLIs agents dans un PTY (mécanisme de transport générique pour tous les agents) |
| `notify` | File watching cross-platform |
| `serde` + `serde_yaml` | `config.yaml`, `subagent-lock.yaml`, frontmatter task/review |
| `uuid` | Génération task-uuid / agent-uuid |
| `chrono` | Timestamps ISO-8601 |
| `strip-ansi-escapes` | Nettoyage du flux PTY pour affichage/parsing texte |
| `anyhow` / `thiserror` | Erreurs |

## Modules

```
src/
  main.rs              — entry point, dispatch clap
  cli/                 — définitions clap par sous-commande
  config.rs            — lecture/écriture ~/.mana/config.yaml
  project.rs           — détection projet (basename pwd), création arborescence .mana/projects/<nom>/
  task.rs              — modèle + parsing task-uuid.md (frontmatter + corps)
  review.rs            — modèle + écriture/lecture review-task-uuid.md
  lock.rs              — lecture/écriture subagent-lock.yaml
  log.rs               — modèle ligne jsonl + writer append-only + lecture dernière ligne (statut)
  pty.rs               — wrapper portable-pty (spawn + reader/writer handles)
  monitor/
    process_watcher.rs — détecte démarrage/fin du process enfant
    file_watcher.rs    — notify sur le dossier projet (fichiers créés/modifiés)
    pty_listener.rs    — parse le flux PTY (strip ANSI + pattern matching best-effort) pour détecter les commandes shell exécutées par l'agent
  tui/
    app.rs             — état global
    chat.rs             — panneau chat (miroir texte ANSI-strippé du PTY du PM)
    graph.rs            — panneau graph (nœuds + statuts + dépendances)
    event.rs             — gestion input clavier
```

## Arborescence des données (`~/.mana/`)

```
.mana/
  config.yaml
  projects/
    <nom-du-repertoire-du-projet>/
      tasks/<task-uuid>.md
      logs/<agent-uuid>.jsonl
      reviews/<task-uuid>.md
      subagent-lock.yaml
```

Détection projet : basename du `pwd` courant. Si `~/.mana/projects/<nom>/` n'existe pas, `mana` crée la structure. Sinon il reprend le contexte existant. Aucune commande d'init nécessaire.

### Formats de fichiers

**`config.yaml`**
```yaml
models:
  <cli-name>:
    name: <cli-name>
    version: x.x.x
    path: /chemin/vers/le/binaire
```

**`tasks/<task-uuid>.md`** — frontmatter YAML + corps Markdown (prompt) :
```markdown
---
id: <uuid>
title: <titre court>
role: <executor | reviewer>
depends-on: [<uuid>, ...]   # optionnel
---

# Corps = prompt injecté au sous-agent
```

**`logs/<agent-uuid>.jsonl`** — une ligne JSON par événement, append-only :
```jsonl
{"status": "running", "action": "started", "timestamp": "..."}
{"status": "running", "action": "cmd:cargo test", "timestamp": "..."}
{"status": "done", "action": "exited", "timestamp": "..."}
```
La dernière ligne = statut courant. Pas de statut "failed" séparé : `done` signifie process terminé, code de sortie inclus dans `action` si non-zéro. Le PM interprète le résultat via le contenu produit (review, diff), pas via un statut d'échec dédié.

**`reviews/<task-uuid>.md`** — rédigé par le reviewer :
- Si validé : uniquement `## Verdict : ✅ Validé` (pas de résumé, pas de prose).
- Si rejeté : `## Verdict : ❌ Rejeté` + liste détaillée des problèmes identifiés.

**`subagent-lock.yaml`** — registre append-only des sous-agents (jamais retiré ; le statut réel se lit dans les logs, pas ici) :
```yaml
<agent-uuid>:
  model: <cli-name>
  role: <executor | reviewer>
  task-uuid: <task-uuid>
```

## Flux principaux

### `mana install`
Sélecteur interactif listant les CLIs connus (Claude, Codex, Gemini, Antigravity, Copilot, Opencode). Tous sélectionnables et enregistrables — `mana install` et le rôle PM (`mana launch <agent>`) sont mécaniquement génériques (spawn PTY + stdin injection + mirror, rien de Claude-spécifique). Pour chaque agent sélectionné : exécute `<cli> --version`, résout le chemin absolu, écrit dans `config.yaml`.

### `mana launch <agent>` (PM)
1. Détecte/crée la structure projet.
2. Spawn `<agent-cli>` dans un PTY interactif normal (comportement standard du CLI, pas de flag spécial).
3. Injecte le prompt PM initial via écriture dans le stdin du PTY (rôle PM, chemin projet, rappel des commandes `mana` disponibles, consigne : ne jamais coder soi-même).
4. Démarre le TUI : chat = miroir ANSI-strippé du flux PTY, graph = vide au départ.
5. Démarre un `file_watcher` (thread séparé) sur `~/.mana/projects/<nom>/`.
6. Tout changement sur `subagent-lock.yaml` ou `logs/*.jsonl` met à jour le graph en temps réel (via channel consommé par la boucle de rendu, tick ~100-200ms).
7. Quand un sous-agent **reviewer** passe à `done` et qu'un fichier apparaît dans `reviews/`, `mana` injecte une notification dans le stdin du PM : `[mana] Review disponible pour <task-uuid> : reviews/<task-uuid>.md`.

### `mana launch --subagent <cli> --role <role> --assign <task-uuid> [params...]`
Commande **synchrone**, invoquée par le PM via son propre outil Bash (process séparé du binaire `mana`, aucune interception nécessaire — la coordination avec le TUI principal se fait entièrement via les fichiers partagés que le `file_watcher` observe) :

1. Vérifie que `tasks/<task-uuid>.md` existe et que toutes les `depends-on` ont le statut `done` (dernière ligne du jsonl correspondant). Sinon : exit non-zero avec message listant les tâches encore en attente.
2. Vérifie que `<cli>` a un mapping de flag "autonome" connu pour le rôle sous-agent (v1 : uniquement `claude` → `--dangerously-skip-permissions` ou équivalent). Sinon : exit non-zero, "rôle sous-agent non supporté pour ce CLI pour l'instant".
3. Génère un `agent-uuid`, ajoute l'entrée dans `subagent-lock.yaml`.
4. Crée `logs/<agent-uuid>.jsonl`, écrit `{"status":"running","action":"started",...}`.
5. Construit le prompt (rôle + contenu du fichier task) et spawn le CLI agent dans un PTY, avec le flag autonome.
6. `pty_listener` parse le flux (best-effort) → append des lignes `cmd:...` à chaque commande shell détectée.
7. `process_watcher` attend la fin du process → append `{"status":"done","action":"exited(...)",...}`.
8. Retourne (exit 0), avec un court résumé en stdout que le PM voit comme résultat de son appel Bash.

Parallélisation : si le PM veut lancer plusieurs sous-agents en même temps, il utilise le mode background natif de son propre outil Bash — `mana` n'a rien de spécial à gérer pour ça, `mana launch --subagent` reste une commande bloquante et autonome.

### Rôle reviewer
Mêmes étapes que ci-dessus, mais le prompt injecté demande de lire `tasks/<task-uuid>.md` + le diff produit par l'executor, puis d'écrire le verdict dans `reviews/<task-uuid>.md` (minimal si validé, détaillé si rejeté — voir format plus haut).

### `mana doctor`
Vérifie : présence des binaires enregistrés, versions à jour (comparaison `<cli> --version`), validité des chemins. Propose des commandes de réparation ou une mise à jour des métadonnées.

### `mana uninstall <cli>`
Retire l'entrée de `config.yaml`. Ne supprime pas le binaire.

### `mana help`
Aide générale ou détaillée par commande (généré par `clap`).

### `mana upgrade`
Hors scope v1 (pas de releases GitHub à consommer pour l'instant). Stub : affiche "pas encore disponible".

## TUI

- Mode par défaut : chat plein écran, input en bas, historique scrollable (texte ANSI-strippé du PTY du PM).
- `Ctrl+G` / `/graph` : toggle split écran (chat gauche / graph droite).
- Graph : nœuds `[PM|EXE|REV] ●/○ <cli>` avec statut lu en direct depuis les jsonl (clignotant = running, fixe = done), liens de dépendance dessinés depuis `depends-on`.
- Aucune sortie brute de sous-agent n'est jamais affichée dans le TUI — seul le statut (via le graph) est visible.

## Gestion d'erreurs

- Agent CLI non enregistré → message clair, suggère `mana install`.
- `--assign` sur un task-uuid inexistant → erreur immédiate.
- Dépendances non satisfaites → erreur explicite listant les tâches en attente.
- CLI sans mapping de flag autonome (v1 : tout sauf `claude`) → erreur explicite, pas de lancement.
- Process agent qui crashe → toujours `done` dans les logs (pas de statut "failed" séparé), code de sortie dans `action`.
- `config.yaml` corrompu → détecté par `mana doctor`, propose réinitialisation.

## Tests

- Unitaires : parsing frontmatter (task/review), résolution des dépendances, lecture statut depuis jsonl, résolution des chemins projet.
- Intégration légère : dossier `.mana/` temporaire (`tempdir`), création d'arborescence, écriture/lecture lock file, détection de dépendances non satisfaites.
- Pas de test end-to-end avec un vrai `claude` en CI (dépendance externe) — validation manuelle en v1.

## Hors scope v1

- `mana upgrade` (self-update).
- Rôle sous-agent pour CLIs autres que `claude`.
- Visualisation de la sortie brute d'un sous-agent depuis le TUI.
- Toute interception/masquage de la commande `mana launch --subagent` dans le chat du PM (elle s'affiche normalement, comme n'importe quel appel Bash).
