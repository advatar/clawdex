use std::process::Command;

use anyhow::Result;

pub fn render_skill(body: &str, args: &str, allow_shell: bool) -> Result<String> {
    let mut rendered = body.to_string();

    for (i, part) in args.split_whitespace().enumerate() {
        rendered = rendered.replace(&format!("$ARGUMENTS[{i}]"), part);
        rendered = rendered.replace(&format!("${i}"), part);
    }
    rendered = rendered.replace("$ARGUMENTS", args);

    if !allow_shell {
        return Ok(rendered);
    }

    let mut output = String::new();
    for line in rendered.lines() {
        if line.trim_start().starts_with('!') {
            let cmd = line.trim_start_matches('!').trim();
            let result = Command::new("sh").arg("-c").arg(cmd).output()?;
            output.push_str(&String::from_utf8_lossy(&result.stdout));
            output.push('\n');
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::render_skill;

    #[test]
    fn render_skill_substitutes_arguments() {
        let out = render_skill("A $ARGUMENTS / $ARGUMENTS[0] / $0", "hello world", false)
            .expect("render");
        assert!(
            out.contains("A hello world / hello / hello"),
            "unexpected output: {out}"
        );
    }

    #[test]
    fn render_skill_preserves_shell_lines_when_disabled() {
        let out = render_skill("!echo test\nbody", "", false).expect("render");
        assert!(out.contains("!echo test"), "expected shell line unchanged");
        assert!(out.contains("body"), "expected body content");
    }
}
