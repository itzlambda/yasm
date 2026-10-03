use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use yasm_core::{GitRef, SourceKind, SourceSpec};
use yasm_providers::fetch_source_cached;

use crate::catalogs::checked_relative;
use crate::model::{MarketplaceRecord, PluginSource};
use crate::state::MarketplacePaths;

pub struct FetchedRoot {
    pub root: Utf8PathBuf,
    pub revision: Option<String>,
}

pub fn fetch_marketplace(source: &SourceSpec, paths: &MarketplacePaths) -> Result<FetchedRoot> {
    let fetched = fetch_source_cached(source, &paths.cache.join("catalogs"))?;
    Ok(FetchedRoot {
        root: fetched.root,
        revision: fetched.resolved.map(|resolved| resolved.commit),
    })
}

pub fn fetch_plugin_source(
    marketplace: &MarketplaceRecord,
    source: &PluginSource,
    paths: &MarketplacePaths,
) -> Result<FetchedRoot> {
    match source {
        PluginSource::Relative { path } => {
            let fetched = fetch_marketplace(&marketplace.source, paths)?;
            Ok(FetchedRoot {
                root: checked_relative(&fetched.root, path)?,
                revision: fetched.revision,
            })
        }
        PluginSource::Git {
            url,
            path,
            r#ref,
            sha,
        } => {
            let checkout = fetch_git(url, r#ref.as_deref(), sha.as_deref(), paths)?;
            let root = match path {
                Some(path) => checked_relative(&checkout.root, path)?,
                None => checkout.root,
            };
            Ok(FetchedRoot {
                root,
                revision: checkout.revision,
            })
        }
    }
}

fn fetch_git(
    url: &str,
    requested_ref: Option<&str>,
    sha: Option<&str>,
    paths: &MarketplacePaths,
) -> Result<FetchedRoot> {
    if sha.is_none() {
        let spec = SourceSpec {
            kind: SourceKind::Github,
            path: url.to_string(),
            r#ref: requested_ref.map(GitRef::parse).transpose()?,
            subpath: None,
        };
        let fetched = fetch_source_cached(&spec, &paths.cache.join("plugins"))?;
        return Ok(FetchedRoot {
            root: fetched.root,
            revision: fetched.resolved.map(|resolved| resolved.commit),
        });
    }

    let checkout = yasm_providers::git::fetch_pinned_checkout(
        &SourceSpec {
            kind: SourceKind::Github,
            path: url.to_string(),
            r#ref: requested_ref.map(GitRef::parse).transpose()?,
            subpath: None,
        },
        sha.context("commit pin is missing")?,
        &paths.cache.join("plugins-pinned"),
    )?;
    Ok(FetchedRoot {
        root: checkout.root,
        revision: checkout.resolved.map(|resolved| resolved.commit),
    })
}
pub fn parse_source(input: &str) -> Result<SourceSpec> {
    input
        .parse::<yasm_providers::SourceInput>()?
        .into_spec()
        .map_err(Into::into)
}
