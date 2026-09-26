use minijinja::{Environment, UndefinedBehavior};
use serde::Serialize;
use std::fmt;

/// Renders `{{ … }}` templates used in env values, commands, domains and the
/// editor command. Undefined variables are errors, so typos surface early.
pub struct Templates {
    env: Environment<'static>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    pub template: String,
    pub message: String,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "in template `{}`: {}", self.template, self.message)
    }
}

impl std::error::Error for TemplateError {}

impl Default for Templates {
    fn default() -> Self {
        Self::new()
    }
}

impl Templates {
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.set_keep_trailing_newline(true);
        Self { env }
    }

    /// Cheap check to skip rendering plain strings.
    pub fn is_template(s: &str) -> bool {
        s.contains("{{") || s.contains("{%") || s.contains("{#")
    }

    pub fn render<S: Serialize>(&self, template: &str, ctx: S) -> Result<String, TemplateError> {
        if !Self::is_template(template) {
            return Ok(template.to_string());
        }
        self.env
            .render_str(template, ctx)
            .map_err(|e| TemplateError {
                template: template.to_string(),
                message: describe(&e),
            })
    }

    pub fn check_syntax(&self, template: &str) -> Result<(), TemplateError> {
        if !Self::is_template(template) {
            return Ok(());
        }
        self.env
            .template_from_str(template)
            .map(|_| ())
            .map_err(|e| TemplateError {
                template: template.to_string(),
                message: describe(&e),
            })
    }
}

fn describe(e: &minijinja::Error) -> String {
    match e.detail() {
        Some(detail) => format!("{} ({detail})", e.kind()),
        None => e.kind().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_and_is_strict() {
        let t = Templates::new();
        let ctx = json!({"cluster": "pr-1", "projects": {"my-api": {"web": {"port": 4100}}}});
        assert_eq!(t.render("{{ cluster }}", &ctx).unwrap(), "pr-1");
        assert_eq!(
            t.render("http://127.0.0.1:{{ projects['my-api'].web.port }}", &ctx)
                .unwrap(),
            "http://127.0.0.1:4100"
        );
        assert_eq!(t.render("plain", &ctx).unwrap(), "plain");
        assert!(t.render("{{ nope }}", &ctx).is_err());
        assert!(t.render("{{ projects.api.web.url }}", &ctx).is_err());
        assert!(t.check_syntax("{{ unclosed").is_err());
    }
}
