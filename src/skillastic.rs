use anyhow::{bail, Context, Result};
use std::env;
use std::process::Command as ProcessCommand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Status,
    List,
    Capture,
}

impl Action {
    pub fn parse(arguments: &[String]) -> Result<Self> {
        match arguments {
            [action] if action == "status" => Ok(Self::Status),
            [action] if action == "list" => Ok(Self::List),
            [action] if action == "capture" => Ok(Self::Capture),
            [] => Ok(Self::Status),
            _ => bail!("skillastic accepts one of: status, list, capture"),
        }
    }

    fn argument(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::List => "list",
            Self::Capture => "capture",
        }
    }
}

pub fn run(action: Action) -> Result<()> {
    let binary = env::var("BUDDY_SKILLASTIC_BIN").unwrap_or_else(|_| "skillastic".to_owned());
    let status = ProcessCommand::new(&binary)
        .arg("--json")
        .arg(action.argument())
        .status()
        .with_context(|| format!("start Skillastic backend '{binary}'"))?;
    if !status.success() {
        bail!("Skillastic backend exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_only_bounded_skillastic_actions() {
        assert_eq!(Action::parse(&strings(&[])).unwrap(), Action::Status);
        assert_eq!(Action::parse(&strings(&["list"])).unwrap(), Action::List);
        assert!(Action::parse(&strings(&["migrate", "--all"])).is_err());
    }
}
