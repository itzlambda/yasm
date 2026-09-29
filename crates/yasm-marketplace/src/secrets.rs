//! Credential checks shared by marketplace import and MCP enablement.

use serde_json::Value;

use crate::model::{
    is_sensitive_env_name, is_sensitive_header_name, is_symbolic_reference,
    literal_credential_in_url, redact_url, McpServerDefinition,
};

const REDACTED: &str = "[redacted]";

pub(crate) fn literal_credential_location(server: &McpServerDefinition) -> Option<String> {
    if let Some((name, _)) = server.env.iter().find(|(name, value)| {
        (is_sensitive_env_name(name) && !is_symbolic_reference(value))
            || text_has_literal_credential_url(value)
    }) {
        return Some(format!("environment variable `{name}`"));
    }
    if let Some((name, _)) = server.headers.iter().find(|(name, value)| {
        (is_sensitive_header_name(name) && !is_symbolic_reference(value))
            || text_has_literal_credential_url(value)
    }) {
        return Some(format!("header `{name}`"));
    }
    if server.url.as_deref().is_some_and(literal_credential_in_url) {
        return Some("its URL".to_string());
    }
    if server
        .command
        .as_deref()
        .is_some_and(text_has_literal_credential_url)
    {
        return Some("its command".to_string());
    }
    if let Some((index, _)) = server
        .args
        .iter()
        .enumerate()
        .find(|(_, arg)| text_has_literal_credential_url(arg))
    {
        return Some(format!("argument {index}"));
    }
    if server
        .cwd
        .as_deref()
        .is_some_and(text_has_literal_credential_url)
    {
        return Some("its working directory".to_string());
    }
    if server
        .extensions
        .iter()
        .any(|(key, value)| nested_has_literal_credential(value, Some(key)))
    {
        return Some("extension data".to_string());
    }
    None
}

pub(crate) fn literal_credential_error(server: &str, location: &str) -> String {
    format!(
        "MCP server `{server}` contains a literal credential in {location}; replace it with an environment or input reference"
    )
}

pub(crate) fn redact_credential_urls_in_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(start) = find_url_start(text, cursor) {
        output.push_str(&text[cursor..start]);
        let end = start + url_end(&text[start..]);
        output.push_str(&redact_url(&text[start..end]));
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

pub(crate) fn redact_nested_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    let value = if is_sensitive_key(key) && !is_symbolic_value(child) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_nested_value(child)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_nested_value).collect()),
        Value::String(text) => Value::String(redact_credential_urls_in_text(text)),
        _ => value.clone(),
    }
}

fn nested_has_literal_credential(value: &Value, key: Option<&str>) -> bool {
    if key.is_some_and(is_sensitive_key) && !is_symbolic_value(value) {
        return true;
    }
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(name, child)| nested_has_literal_credential(child, Some(name))),
        Value::Array(items) => items
            .iter()
            .any(|child| nested_has_literal_credential(child, None)),
        Value::String(text) => text_has_literal_credential_url(text),
        _ => false,
    }
}

pub(crate) fn value_has_literal_credential(value: &Value) -> bool {
    nested_has_literal_credential(value, None)
}

pub(crate) fn keyed_value_has_literal_credential(key: &str, value: &Value) -> bool {
    nested_has_literal_credential(value, Some(key))
}

fn is_symbolic_value(value: &Value) -> bool {
    value.as_str().is_some_and(is_symbolic_reference)
}

fn is_sensitive_key(key: &str) -> bool {
    is_sensitive_env_name(key)
        || is_sensitive_header_name(key)
        || matches!(
            key.to_ascii_lowercase().as_str(),
            "key" | "sig" | "signature"
        )
}

fn text_has_literal_credential_url(text: &str) -> bool {
    let mut cursor = 0;
    while let Some(start) = find_url_start(text, cursor) {
        let end = start + url_end(&text[start..]);
        if literal_credential_in_url(&text[start..end]) {
            return true;
        }
        cursor = end;
    }
    false
}

fn find_url_start(text: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(relative) = text[search..].find("://") {
        let separator = search + relative;
        let mut start = separator;
        for (index, character) in text[..separator].char_indices().rev() {
            if character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.') {
                start = index;
            } else {
                break;
            }
        }
        if start < separator
            && text[start..separator].starts_with(|character: char| character.is_ascii_alphabetic())
        {
            return Some(start);
        }
        search = separator + 3;
    }
    None
}

fn url_end(text: &str) -> usize {
    text.find(|character: char| {
        character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '`')
    })
    .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use crate::model::McpServerDefinition;
    use crate::secrets::{
        literal_credential_location, redact_credential_urls_in_text,
        text_has_literal_credential_url,
    };

    #[test]
    fn embedded_urls_keep_symbolic_references_and_mask_unknown_query_values() {
        let symbolic = "--endpoint=https://example.test/mcp?token=${input:token}";
        assert_eq!(redact_credential_urls_in_text(symbolic), symbolic);
        assert!(!text_has_literal_credential_url(symbolic));

        let unknown = "--endpoint=https://example.test/mcp?custom=top-secret";
        assert_eq!(
            redact_credential_urls_in_text(unknown),
            "--endpoint=https://example.test/mcp?custom=[redacted]"
        );

        let bare = "--endpoint=https://example.test/mcp?opaque123";
        assert_eq!(
            redact_credential_urls_in_text(bare),
            "--endpoint=https://example.test/mcp?[redacted]"
        );
        assert_eq!(
            redact_credential_urls_in_text("https://example.test/mcp?${env:TOKEN}"),
            "https://example.test/mcp?${env:TOKEN}"
        );
    }

    #[test]
    fn ordinary_mcp_value_names_do_not_exempt_credential_urls() {
        for (field, location) in [
            ("env", "environment variable `ENDPOINT`"),
            ("headers", "header `ENDPOINT`"),
        ] {
            let mut raw = serde_json::json!({"name": "private", "transport": "http"});
            raw[field] = serde_json::json!({"ENDPOINT": "https://example.test/mcp?sig=top-secret"});
            let server: McpServerDefinition = serde_json::from_value(raw).unwrap();
            assert_eq!(
                literal_credential_location(&server).as_deref(),
                Some(location)
            );
        }
    }
}
