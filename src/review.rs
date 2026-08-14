use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Validated,
    Rejected { problems: Vec<String> },
}

pub fn render_review(task_uuid: &str, verdict: &Verdict) -> String {
    match verdict {
        Verdict::Validated => format!("## Verdict : \u{2705} Valid\u{e9}\n"),
        Verdict::Rejected { problems } => {
            let mut out = format!(
                "# Review — {task_uuid}\n\n## Verdict : \u{274c} Rejet\u{e9}\n\n### Probl\u{e8}mes identifi\u{e9}s\n\n"
            );
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
        let mut problems = Vec::new();
        let mut found_problems_section = false;

        for line in contents.lines() {
            // Check if this line marks the problems section
            if line.contains("Probl\u{e8}mes identifi\u{e9}s") {
                found_problems_section = true;
                continue;
            }

            // Only process lines after the problems section header
            if !found_problems_section {
                continue;
            }

            let trimmed = line.trim();

            // Skip empty lines
            if trimmed.is_empty() {
                continue;
            }

            // Check if this line starts a new problem (matches N. format)
            if let Some(dot_pos) = trimmed.find(". ") {
                let prefix = &trimmed[..dot_pos];
                if prefix.chars().all(|c| c.is_ascii_digit()) && !prefix.is_empty() {
                    // This is a new problem
                    let problem_text = trimmed[dot_pos + 2..].to_string();
                    problems.push(problem_text);
                    continue;
                }
            }

            // If we have existing problems, treat this line as a continuation
            if !problems.is_empty() {
                if let Some(last) = problems.last_mut() {
                    last.push(' ');
                    last.push_str(trimmed);
                }
            }
        }

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
        // Per spec: validated reviews must be ONLY the verdict line, no title or extra prose
        assert_eq!(rendered, "## Verdict : \u{2705} Valid\u{e9}\n");
    }

    #[test]
    fn rejected_review_lists_problems() {
        let verdict = Verdict::Rejected {
            problems: vec![
                "Gestion d'erreur manquante".to_string(),
                "Test incomplet".to_string(),
            ],
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
        let verdict = Verdict::Rejected {
            problems: vec!["Probleme A".to_string(), "Probleme B".to_string()],
        };
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

    #[test]
    fn multiline_problem_text_roundtrips() {
        // Test that a problem with an embedded newline (multi-line text) is recovered
        // as a single-line string with continuation joined by space.
        // If we manually construct a scenario with line breaks, parse should handle it
        let manual_content = "# Review — task-1\n\n## Verdict : \u{274c} Rejet\u{e9}\n\n### Probl\u{e8}mes identifi\u{e9}s\n\n1. First line of problem description\nSecond line continues here\n";
        let parsed = parse_verdict(manual_content).unwrap();

        // The continuation line should be appended with a space
        if let Verdict::Rejected { problems } = parsed {
            assert_eq!(problems.len(), 1);
            assert_eq!(
                problems[0],
                "First line of problem description Second line continues here"
            );
        } else {
            panic!("expected Rejected verdict");
        }
    }

    #[test]
    fn problems_section_scoping_ignores_numbered_lines_before_section() {
        // Test that numbered lines appearing BEFORE the "Problèmes identifiés" section
        // are NOT picked up as problems
        let content = "# Review — task-1\n\n1. This looks like a problem but isn't\n\n## Verdict : \u{274c} Rejet\u{e9}\n\n### Probl\u{e8}mes identifi\u{e9}s\n\n1. Real problem\n";
        let parsed = parse_verdict(content).unwrap();

        if let Verdict::Rejected { problems } = parsed {
            // Should only have the real problem, not the numbered line before the section
            assert_eq!(problems.len(), 1);
            assert_eq!(problems[0], "Real problem");
        } else {
            panic!("expected Rejected verdict");
        }
    }
}
