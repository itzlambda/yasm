use anyhow::Result;
use camino::Utf8PathBuf;
use yasm_core::{GitRef, SourceSpec};
use yasm_providers::fetch_source_cached;
use yasm_providers::git::{fetch_pinned_checkout, GitRemote};

use crate::catalogs::checked_relative;
use crate::model::{MarketplaceRecord, PluginSource};
use crate::state::MarketplacePaths;

pub struct FetchedRoot {
    pub root: Utf8PathBuf,
    pub revision: Option<String>,
    _lease: Option<yasm_providers::git::CheckoutLease>,
}

pub fn fetch_marketplace(source: &SourceSpec, paths: &MarketplacePaths) -> Result<FetchedRoot> {
    let fetched = fetch_source_cached(source, &paths.cache.join("catalogs"))?;
    let lease = fetched.checkout_lease();
    Ok(FetchedRoot {
        root: fetched.root,
        revision: fetched.resolved.map(|resolved| resolved.commit),
        _lease: lease,
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
            let lease = fetched._lease.clone();
            Ok(FetchedRoot {
                root: checked_relative(&fetched.root, path)?,
                revision: fetched.revision,
                _lease: lease,
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
                _lease: checkout._lease,
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
    let mut spec: SourceSpec = url.parse::<GitRemote>()?.into();
    spec.r#ref = requested_ref.map(GitRef::parse).transpose()?;
    let checkout = match sha {
        Some(sha) => fetch_pinned_checkout(&spec, sha, &paths.cache.join("plugins-pinned"))?,
        None => yasm_providers::git::fetch_checkout_cached(&spec, &paths.cache.join("plugins"))?,
    };
    let lease = checkout.checkout_lease();
    Ok(FetchedRoot {
        root: checkout.root,
        revision: checkout.resolved.map(|resolved| resolved.commit),
        _lease: lease,
    })
}

pub fn parse_source(input: &str) -> Result<SourceSpec> {
    input
        .parse::<yasm_providers::SourceInput>()?
        .into_spec()
        .map_err(Into::into)
}
