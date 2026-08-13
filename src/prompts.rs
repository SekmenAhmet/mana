use crate::task::Task;
use std::path::Path;

pub fn pm_prompt(project_name: &str) -> String {
    format!(
        "Tu es le Main Agent (Project Manager) orchestre par mana pour le projet '{project_name}'.\n\
        Tu ne codes jamais toi-meme. Ton role : discuter avec l'utilisateur pour comprendre le besoin, \
        decouper le travail en taches, et deleguer chaque tache a un sous-agent via la commande shell \
        `mana launch --subagent <cli> --role <executor|reviewer> --assign <task-uuid>`.\n\n\
        Pour creer une tache, ecris un fichier `.mana/projects/{project_name}/tasks/<uuid>.md` avec un \
        frontmatter YAML (id, title, role, depends-on optionnel) suivi du prompt destine au sous-agent.\n\n\
        Lance d'abord un executor pour chaque tache de code, puis un reviewer une fois l'executor termine, \
        en lui assignant la meme tache. Lis le fichier de review ecrit dans `.mana/projects/{project_name}/reviews/<uuid>.md` \
        pour decider si la tache est terminee ou si elle doit etre relancee avec des corrections.\n\n\
        Commence par poser des questions a l'utilisateur pour bien comprendre ce qu'il veut construire."
    )
}

pub fn executor_prompt(task: &Task, task_path: &Path) -> String {
    format!(
        "Tu es un sous-agent executor orchestre par mana. Ta tache est decrite dans {} (id: {}, titre: {}).\n\n\
        Realise exactement ce qui est demande : ecris le code, ajoute des tests si pertinent, verifie que ca compile/passe. \
        Quand c'est termine, arrete-toi simplement — n'attends aucune confirmation, personne ne repondra.\n\n\
        --- Contenu de la tache ---\n{}",
        task_path.display(),
        task.frontmatter.id,
        task.frontmatter.title,
        task.body,
    )
}

pub fn reviewer_prompt(task: &Task, task_path: &Path, review_path: &Path) -> String {
    format!(
        "Tu es un sous-agent reviewer orchestre par mana. Relis le travail realise pour la tache decrite dans {} \
        (id: {}, titre: {}), en comparant avec les changements de code produits (regarde le diff git le cas echeant).\n\n\
        Ecris ton verdict dans {} :\n\
        - Si tout est conforme, ecris UNIQUEMENT la ligne `## Verdict : \u{2705} Valid\u{e9}` — pas de resume, pas de prose supplementaire.\n\
        - Si tu trouves des problemes, ecris `## Verdict : \u{274c} Rejet\u{e9}` suivi d'une section `### Problemes identifies` \
        avec une liste numerotee, un probleme concret par ligne.\n\n\
        --- Contenu de la tache ---\n{}",
        task_path.display(),
        task.frontmatter.id,
        task.frontmatter.title,
        review_path.display(),
        task.body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Role, TaskFrontmatter};
    use std::path::PathBuf;

    fn sample_task() -> Task {
        Task {
            frontmatter: TaskFrontmatter {
                id: "uuid-1".to_string(),
                title: "Titre de test".to_string(),
                role: Role::Executor,
                depends_on: vec![],
            },
            body: "# Description\n\nFais X.\n".to_string(),
        }
    }

    #[test]
    fn pm_prompt_mentions_project_and_launch_command() {
        let prompt = pm_prompt("mon-api");
        assert!(prompt.contains("mon-api"));
        assert!(prompt.contains("mana launch --subagent"));
        assert!(prompt.contains("ne codes jamais"));
    }

    #[test]
    fn executor_prompt_includes_task_body_and_path() {
        let task = sample_task();
        let path = PathBuf::from("/tmp/tasks/uuid-1.md");
        let prompt = executor_prompt(&task, &path);
        assert!(prompt.contains("Fais X."));
        assert!(prompt.contains("uuid-1.md"));
        assert!(prompt.contains("Titre de test"));
    }

    #[test]
    fn reviewer_prompt_explains_minimal_validated_format() {
        let task = sample_task();
        let task_path = PathBuf::from("/tmp/tasks/uuid-1.md");
        let review_path = PathBuf::from("/tmp/reviews/uuid-1.md");
        let prompt = reviewer_prompt(&task, &task_path, &review_path);
        assert!(prompt.contains("UNIQUEMENT la ligne"));
        assert!(prompt.contains("Probleme"));
        assert!(prompt.contains("uuid-1.md"));
    }
}
