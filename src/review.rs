use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Validated,
    Rejected { problems: Vec<String> },
}

pub fn render_review(task_uuid: &str, verdict: &Verdict) -> String {
    match verdict {
        Verdict::Validated => format!("# Review — {task_uuid}\n\n## Verdict : \u{2705} Valid\u{e9}\n"),
        Verdict::Rejected { problems } => {
            let mut out = format!("# Review — {task_uuid}\n\n## Verdict : \u{274c} Rejet\u{e9}\n\n### Probl\u{e8}mes identifi\u{e9}s\n\n");
            for (i, problem) in problems.iter().enumerate() {
                out.push_str(&format!("{}. {}\n", i + 1, problem));
            }
            out
        }
    }
}

pub fn write_review(path: &Path, task_uuid: &str, verdict: &Verdict) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, render_review(task_uuid, verdict))?;
    Ok(())
}

pub fn parse_verdict(contents: &str) -> anyhow::Result<Verdict> {
    if contents.contains("\u{2705} Valid\u{e9}") {
        return Ok(Verdict::Validated);
    }
    if contents.contains("\u{274c} Rejet\u{e9}") {
        let problems = contents
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                let rest = trimmed.split_once(". ")?;
                if rest.0.chars().all(|c| c.is_ascii_digit()) && !rest.0.is_empty() {
                    Some(rest.1.to_string())
                } else {
                    None
                }
            })
            .collect();
        return Ok(Verdict::Rejected { problems });
    }
    anyhow::bail!("no recognizable verdict found in review contents")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_review_has_no_extra_prose() {
        let rendered = render_review("task-1", &Verdict::Validated);
        assert!(rendered.contains("Valid\u{e9}"));
        assert!(!rendered.contains("conform"));
    }

    #[test]
    fn rejected_review_lists_problems() {
        let verdict = Verdict::Rejected {
            problems: vec!["Gestion d'erreur manquante".to_string(), "Test incomplet".to_string()],
        };
        let rendered = render_review("task-1", &verdict);
        assert!(rendered.contains("1. Gestion d'erreur manquante"));
        assert!(rendered.contains("2. Test incomplet"));
    }

    #[test]
    fn parse_verdict_roundtrips_validated() {
        let rendered = render_review("task-1", &Verdict::Validated);
        assert_eq!(parse_verdict(&rendered).unwrap(), Verdict::Validated);
    }

    #[test]
    fn parse_verdict_roundtrips_rejected() {
        let verdict = Verdict::Rejected { problems: vec!["Probleme A".to_string(), "Probleme B".to_string()] };
        let rendered = render_review("task-1", &verdict);
        assert_eq!(parse_verdict(&rendered).unwrap(), verdict);
    }

    #[test]
    fn write_then_parse_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("reviews/task-1.md");
        write_review(&path, "task-1", &Verdict::Validated).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_verdict(&contents).unwrap(), Verdict::Validated);
    }
}
