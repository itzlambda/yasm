use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, MultiSelect, Select};

pub fn ask_multiselect(
    field: &str,
    prompt: &str,
    labels: &[String],
    defaults: &[bool],
    hint: &str,
) -> Result<Vec<usize>> {
    if !std::io::stdin().is_terminal() {
        bail!("missing input for {field}; {hint}");
    }

    let theme = ColorfulTheme::default();
    MultiSelect::with_theme(&theme)
        .with_prompt(prompt)
        .items(labels)
        .defaults(defaults)
        .interact()
        .map_err(Into::into)
}

pub fn ask_confirm(field: &str, prompt: &str, default: bool, hint: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("missing input for {field}; {hint}");
    }

    let theme = ColorfulTheme::default();
    Confirm::with_theme(&theme)
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(Into::into)
}

pub fn ask_select(field: &str, prompt: &str, labels: &[String], hint: &str) -> Result<usize> {
    if !std::io::stdin().is_terminal() {
        bail!("missing input for {field}; {hint}");
    }

    let theme = ColorfulTheme::default();
    Select::with_theme(&theme)
        .with_prompt(prompt)
        .items(labels)
        .default(0)
        .interact()
        .map_err(Into::into)
}

pub fn ask_input(field: &str, prompt: &str, hint: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("missing input for {field}; {hint}");
    }

    let theme = ColorfulTheme::default();
    Input::<String>::with_theme(&theme)
        .with_prompt(prompt)
        .interact_text()
        .map_err(Into::into)
}
