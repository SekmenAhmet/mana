use anyhow::{Context, Result, bail};
use std::collections::HashMap;

/// The complete set of placeholders a catalogue argv template may use. Closed
/// on purpose: an unrecognised `{token}` is a typo, and a typo that survived
/// into an argv array would be handed to the CLI verbatim.
pub const KNOWN_PLACEHOLDERS: &[&str] = &["model", "prompt", "config_path", "cwd"];

/// A template argument is a flat sequence of literal text and placeholders.
/// Splitting it once and reusing the result is what keeps `substitute` from
/// ever rescanning a value it just wrote -- a prompt containing `{foo}` must
/// come out untouched, not be mistaken for a placeholder.
enum Segment<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
}

fn split(arg: &str) -> Result<Vec<Segment<'_>>> {
    let mut segments = Vec::new();
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            segments.push(Segment::Literal(&rest[..open]));
        }
        let after_open = &rest[open + 1..];
        let close = after_open.find('}').ok_or_else(|| {
            anyhow::anyhow!("unterminated placeholder in {arg:?}: '{{' with no closing '}}'")
        })?;
        segments.push(Segment::Placeholder(&after_open[..close]));
        rest = &after_open[close + 1..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Literal(rest));
    }
    Ok(segments)
}

fn ensure_known(name: &str, arg: &str) -> Result<()> {
    if KNOWN_PLACEHOLDERS.contains(&name) {
        return Ok(());
    }
    bail!(
        "unknown placeholder '{{{name}}}' in {arg:?} (known placeholders: {})",
        KNOWN_PLACEHOLDERS.join(", ")
    )
}

/// Checks a template without needing any values. Run over every argv template
/// at load time so a bad placeholder fails `cargo test` on the maintainer's
/// machine instead of failing a dispatch on the user's.
///
/// `context` names the field being checked; it is attached to any error so a
/// failure points at the offending field, not just at the offending string.
pub fn validate_placeholders(template: &[String], context: &str) -> Result<()> {
    check(template).with_context(|| context.to_string())
}

fn check(template: &[String]) -> Result<()> {
    for arg in template {
        for segment in split(arg)? {
            if let Segment::Placeholder(name) = segment {
                ensure_known(name, arg)?;
            }
        }
    }
    Ok(())
}

/// Fills a catalogue argv template. mana substitutes and spawns the result
/// directly -- no shell ever sees catalogue content, so nothing in a value
/// can be interpreted as syntax.
///
/// A missing value is an error rather than a pass-through: leaving a literal
/// `{model}` in argv would send it to the CLI as if it were a real argument.
pub fn substitute(template: &[String], vars: &HashMap<&str, &str>) -> Result<Vec<String>> {
    let mut filled = Vec::with_capacity(template.len());
    for arg in template {
        let mut out = String::with_capacity(arg.len());
        for segment in split(arg)? {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Placeholder(name) => {
                    ensure_known(name, arg)?;
                    let value = vars.get(name).with_context(|| {
                        format!("no value supplied for '{{{name}}}' in {arg:?}")
                    })?;
                    out.push_str(value);
                }
            }
        }
        filled.push(out);
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn substitutes_every_known_placeholder() {
        let vars = HashMap::from([
            ("model", "gemini-3.7-flash-low"),
            ("prompt", "do the thing"),
            ("config_path", "/tmp/mcp.json"),
            ("cwd", "/repo"),
        ]);
        let filled = substitute(
            &template(&[
                "--model",
                "{model}",
                "--mcp-config",
                "{config_path}",
                "--cwd={cwd}",
                "{prompt}",
            ]),
            &vars,
        )
        .unwrap();
        assert_eq!(
            filled,
            vec![
                "--model",
                "gemini-3.7-flash-low",
                "--mcp-config",
                "/tmp/mcp.json",
                "--cwd=/repo",
                "do the thing",
            ]
        );
    }

    #[test]
    fn leaves_templates_without_placeholders_alone() {
        let filled = substitute(&template(&["-p", "--verbose"]), &HashMap::new()).unwrap();
        assert_eq!(filled, vec!["-p", "--verbose"]);
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let err =
            substitute(&template(&["--session", "{session_id}"]), &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
        assert!(err.to_string().contains("known placeholders"), "{err}");
    }

    #[test]
    fn missing_value_is_an_error_not_a_passthrough() {
        let err = substitute(&template(&["--model", "{model}"]), &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[test]
    fn unterminated_placeholder_is_an_error() {
        let err = substitute(&template(&["{model"]), &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn substituted_values_are_never_rescanned() {
        // A user prompt is free text and may well contain braces; it must come
        // out as written rather than be mistaken for a nested placeholder.
        let vars = HashMap::from([("prompt", "print {model} literally")]);
        let filled = substitute(&template(&["{prompt}"]), &vars).unwrap();
        assert_eq!(filled, vec!["print {model} literally"]);
    }

    #[test]
    fn validate_accepts_known_placeholders() {
        assert!(validate_placeholders(&template(&["--model", "{model}"]), "field").is_ok());
    }

    #[test]
    fn validate_rejects_unknown_placeholder_and_names_the_field() {
        let err =
            validate_placeholders(&template(&["{modle}"]), "[subagent].model_args").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("modle"), "{rendered}");
        assert!(rendered.contains("[subagent].model_args"), "{rendered}");
    }
}
