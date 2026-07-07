use crate::model::{Action, CommandLine, PackageDetail, PackageHit, SourceId, SourceMeta};
use crate::sources::Source;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::process::Command;

/// By default dnf loads filelists, comps, other, and updateinfo metadata on top
/// of the primary metadata. Search and version lookup need only primary (names,
/// summaries, versions, repos), so skipping the rest cuts each metadata load by
/// roughly a third. Passed as a global option before the subcommand. Not used
/// for `--providers-of=requires` (the detail dependency list), which needs
/// filelists to resolve file-based requires.
pub const PRIMARY_ONLY: &str = "--setopt=optional_metadata_types=";

/// The full repo catalog, loaded once in the background so searches can filter
/// it in memory instead of shelling out to dnf (a multi-second pool rebuild) per
/// query. `None` until the first successful load; searches fall back to the live
/// `dnf search` path while it is `None`.
pub struct DnfSource {
    catalog: Arc<RwLock<Option<Arc<Vec<PackageHit>>>>>,
    loading: Arc<AtomicBool>,
}

impl DnfSource {
    pub fn new() -> Self {
        DnfSource { catalog: Arc::new(RwLock::new(None)), loading: Arc::new(AtomicBool::new(false)) }
    }
}

impl Default for DnfSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Dump the whole repo catalog (one latest row per package: name, evr, repoid,
/// summary) with primary metadata only. `None` on spawn failure, non-zero exit,
/// or an empty result, so a failed load never replaces a good catalog with junk.
async fn dump_catalog() -> Option<Vec<PackageHit>> {
    let out = Command::new("dnf")
        .env("LC_ALL", "C")
        .arg(PRIMARY_ONLY)
        .args([
            "repoquery",
            "--qf",
            "%{name}\t%{evr}\t%{repoid}\t%{summary}\n",
            "--latest-limit=1",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hits = parse_catalog(&String::from_utf8_lossy(&out.stdout));
    (!hits.is_empty()).then_some(hits)
}

#[async_trait]
impl Source for DnfSource {
    fn id(&self) -> SourceId {
        SourceId::Dnf
    }

    fn display_name(&self) -> &'static str {
        "dnf"
    }

    fn warm(&self) {
        // (Re)load the full catalog in the background. Called once at startup and
        // again after an in-plaza upgrade. `loading` guards against overlapping
        // loads; a refresh keeps serving the old catalog until the new one lands.
        if self.loading.swap(true, Ordering::SeqCst) {
            return;
        }
        let catalog = Arc::clone(&self.catalog);
        let loading = Arc::clone(&self.loading);
        tokio::spawn(async move {
            if let Some(hits) = dump_catalog().await {
                *catalog.write().unwrap() = Some(Arc::new(hits));
            }
            loading.store(false, Ordering::SeqCst);
        });
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<PackageHit>> {
        // Fast path: once the background catalog is loaded, filter it in memory
        // instead of shelling out. Clone the Arc so the lock is released before
        // filtering.
        let ready = self.catalog.read().unwrap().clone();
        if let Some(catalog) = ready {
            return Ok(filter_catalog(&catalog, query));
        }
        // Live fallback until the catalog is ready (and for headless `--search`,
        // which never warms).
        // `dnf search` matches name + summary but gives no version or repo; a
        // `dnf repoquery` fills those in. Each call loads all repo metadata into
        // libsolv (seconds of CPU), so instead of searching and then querying
        // the matched names in series, both run concurrently: the repoquery uses
        // a `*query*` name glob independent of the search output. Both load only
        // primary metadata (`PRIMARY_ONLY`). Together this roughly halves search
        // latency. A summary-only match falls outside the glob and is kept
        // without a version (see `build_hits`).
        let glob = format!("*{query}*");
        let search_fut = Command::new("dnf")
            .env("LC_ALL", "C")
            .arg(PRIMARY_ONLY)
            .arg("search")
            .arg(query)
            .output();
        let repoquery_fut = Command::new("dnf")
            .env("LC_ALL", "C")
            .arg(PRIMARY_ONLY)
            .args(["repoquery", "--qf", "%{name}\t%{evr}\t%{repoid}\n", "--latest-limit=1"])
            .arg(&glob)
            .output();
        let (search_out, repoquery_out) = tokio::join!(search_fut, repoquery_fut);
        let search = String::from_utf8_lossy(&search_out?.stdout).into_owned();
        let repoquery = String::from_utf8_lossy(&repoquery_out?.stdout).into_owned();
        Ok(build_hits(&search, &repoquery))
    }

    fn action_command(&self, _action: Action, pkg: &str) -> CommandLine {
        CommandLine {
            program: "sudo".into(),
            args: vec!["dnf".into(), "install".into(), pkg.into()],
        }
    }
}

/// Strip a trailing `.arch` suffix from a `name.arch` token. dnf lists packages
/// as `name.x86_64` / `name.noarch`; repoquery `%{name}` is arch-less, so the
/// search names are trimmed to match. A token with no dot is returned unchanged.
fn strip_arch(token: &str) -> &str {
    token.rsplit_once('.').map(|(n, _)| n).unwrap_or(token)
}

/// Parse `dnf search <q>` output into `(name, summary)` pairs. dnf5 prints each
/// match as `name.arch<TAB>summary` (indented by a space); dnf4 used
/// `name.arch : summary`. Section headers ("Matched fields: name (exact)") and
/// the repo-loading chatter (dnf writes that to stderr anyway) carry whitespace
/// in the part before the separator, which a package `name.arch` never does, so
/// they are skipped.
pub fn parse_search_output(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            // dnf5 separates with a tab; fall back to the dnf4 colon form.
            let (left, summary) = line.split_once('\t').or_else(|| line.split_once(':'))?;
            let left = left.trim();
            if left.is_empty() || left.contains(char::is_whitespace) || !left.contains('.') {
                return None;
            }
            Some((strip_arch(left).to_string(), summary.trim().to_string()))
        })
        .collect()
}

/// Parse `dnf repoquery --qf '%{name}\t%{evr}\t%{repoid}\n'` into a map of
/// name -> (evr, repoid). The first line for a name wins (repoquery is run
/// with `--latest-limit=1`, so that is the newest candidate). `%{repoid}` is the
/// short repo id ("fedora", "updates"), not the pretty `%{reponame}`.
pub fn parse_repoquery(output: &str) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for line in output.lines() {
        let mut parts = line.split('\t');
        let (Some(name), Some(evr), Some(repo)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        map.entry(name.to_string())
            .or_insert_with(|| (evr.trim().to_string(), repo.trim().to_string()));
    }
    map
}

/// Join the `dnf search` match set with repoquery versions/repos into dnf
/// PackageHits. The repoquery runs as a name glob in parallel with the search,
/// so a hit matched only by its summary has no candidate; it is kept with an
/// empty version and no repo (the detail view fills that in on selection), not
/// dropped.
pub fn build_hits(search: &str, repoquery: &str) -> Vec<PackageHit> {
    let rq = parse_repoquery(repoquery);
    parse_search_output(search)
        .into_iter()
        .map(|(name, description)| {
            let (version, repo) = match rq.get(&name) {
                Some((v, r)) => (v.clone(), Some(r.clone())),
                None => (String::new(), None),
            };
            PackageHit {
                name,
                version,
                source_id: SourceId::Dnf,
                description,
                meta: SourceMeta { repo, maintained: true, ..Default::default() },
            }
        })
        .collect()
}

/// Parse the full-catalog dump (`dnf repoquery --qf
/// '%{name}\t%{evr}\t%{repoid}\t%{summary}\n'`) into dnf PackageHits. Each line
/// is four tab fields; lines without all four (or with an empty name) are
/// skipped. `%{name}` carries no `.arch` suffix, so none is stripped.
pub fn parse_catalog(output: &str) -> Vec<PackageHit> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let (name, evr, repo, summary) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(PackageHit {
                name: name.to_string(),
                version: evr.trim().to_string(),
                source_id: SourceId::Dnf,
                description: summary.trim().to_string(),
                meta: SourceMeta {
                    repo: Some(repo.trim().to_string()),
                    maintained: true,
                    ..Default::default()
                },
            })
        })
        .collect()
}

/// Keep catalog hits whose name or summary contains `query`, case-insensitive.
/// Mirrors how `dnf search` matches against name and summary.
pub fn filter_catalog(catalog: &[PackageHit], query: &str) -> Vec<PackageHit> {
    let q = query.to_lowercase();
    catalog
        .iter()
        .filter(|h| {
            h.name.to_lowercase().contains(&q) || h.description.to_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

/// Parse `dnf info <pkg>` (aligned `Key : value`, continuation lines whose part
/// before the colon is blank) into a `PackageDetail`. The first block wins.
/// `depends` is filled separately from `dnf repoquery --requires`.
pub fn parse_info_output(text: &str) -> PackageDetail {
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        let Some((left, val)) = line.split_once(':') else {
            continue;
        };
        let key = left.trim();
        if key.is_empty() {
            // continuation of the previous field's value
            if let Some(k) = &last {
                let e = fields.entry(k.clone()).or_default();
                e.push(' ');
                e.push_str(val.trim());
            }
            continue;
        }
        fields.entry(key.to_string()).or_insert_with(|| val.trim().to_string());
        last = Some(key.to_string());
    }
    let val = |k: &str| fields.get(k).filter(|s| !s.is_empty()).cloned();
    PackageDetail {
        url: val("URL"),
        repo_url: None,
        licenses: val("License"),
        install_size: val("Installed size"),
        build_date: None,
        depends: Vec::new(),
        optional_depends: Vec::new(),
        maintainer: None,
        popularity: None,
    }
}

/// Parse `dnf repoquery --providers-of=requires --qf '%{name}\n'` (one dependency
/// package name per line) into a deduped list. dnf5 dropped `--requires --resolve`
/// and forbids `--qf` with `--requires`, so `--providers-of=requires` is the way
/// to resolve requirements to package names. Any version constraint or file path
/// fragment after the bare name token is dropped; blank lines are ignored.
pub fn parse_requires(output: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in output.lines() {
        let name = line.split([' ', '(']).next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_names_and_skips_headers() {
        // Real dnf5 output: tab-separated, package lines indented by one space,
        // with "Matched fields:" section headers between the groups.
        let out = "\
Matched fields: name (exact)
 ripgrep.x86_64\tLine-oriented search tool
Matched fields: name, summary
 ripgrep-edit.x86_64\tEdit ripgrep search results across multiple files
 ripgrep-edit-emacs.noarch\tUse Emacs to edit ripgrep search results
";
        let hits = parse_search_output(out);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0], ("ripgrep".to_string(), "Line-oriented search tool".to_string()));
        assert_eq!(hits[1].0, "ripgrep-edit");
        assert_eq!(hits[2].0, "ripgrep-edit-emacs");
        assert!(parse_search_output("").is_empty());
    }

    #[test]
    fn parses_search_dnf4_colon_form() {
        // dnf4 used "name.arch : summary"; the colon fallback still handles it.
        let out = "\
vim-enhanced.x86_64: A version of the VIM editor which includes: extras
protobuf-vim.noarch : Vim syntax highlighting
";
        let hits = parse_search_output(out);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "vim-enhanced");
        // only the first colon splits, so a colon in the summary is preserved
        assert_eq!(hits[0].1, "A version of the VIM editor which includes: extras");
        assert_eq!(hits[1].0, "protobuf-vim");
    }

    #[test]
    fn parses_repoquery_first_per_name() {
        let out = "vim-enhanced\t2:9.1.158-1.fc41\tupdates\nvim-enhanced\t2:9.1.100-1.fc41\tfedora\n";
        let m = parse_repoquery(out);
        assert_eq!(m.get("vim-enhanced").unwrap(), &("2:9.1.158-1.fc41".to_string(), "updates".to_string()));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn build_hits_joins_and_keeps_unmatched() {
        // The version/repo repoquery runs as a name glob in parallel with the
        // search, so a summary-only match has no repoquery candidate. Such a hit
        // is kept (it is a real package dnf returned) with an empty version and
        // no repo badge, not dropped; the detail view fills its version in on
        // selection.
        let search = "vim-enhanced.x86_64\tEnhanced vi\norphan.noarch\tnot in repoquery\n";
        let repoquery = "vim-enhanced\t2:9.1.158-1.fc41\tupdates\n";
        let hits = build_hits(search, repoquery);
        assert_eq!(hits.len(), 2);
        let vim = &hits[0];
        assert_eq!(vim.name, "vim-enhanced");
        assert_eq!(vim.version, "2:9.1.158-1.fc41");
        assert_eq!(vim.source_id, SourceId::Dnf);
        assert_eq!(vim.description, "Enhanced vi");
        assert_eq!(vim.meta.repo.as_deref(), Some("updates"));
        assert!(vim.meta.maintained);
        let orphan = &hits[1];
        assert_eq!(orphan.name, "orphan");
        assert_eq!(orphan.version, "");
        assert_eq!(orphan.meta.repo, None);
        assert!(orphan.meta.maintained);
    }

    #[test]
    fn parses_catalog_rows_and_skips_malformed() {
        let out = "\
ripgrep\t14.1.1-3.fc43\tfedora\tLine-oriented search tool
vim-enhanced\t2:9.1.100-1.fc43\tupdates\tThe VIM editor
empty-summary\t1-1.fc43\tfedora\t
bad line without tabs
\t1-1\tfedora\tblank name is skipped
";
        let hits = parse_catalog(out);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].name, "ripgrep");
        assert_eq!(hits[0].version, "14.1.1-3.fc43");
        assert_eq!(hits[0].source_id, SourceId::Dnf);
        assert_eq!(hits[0].description, "Line-oriented search tool");
        assert_eq!(hits[0].meta.repo.as_deref(), Some("fedora"));
        assert!(hits[0].meta.maintained);
        assert_eq!(hits[1].name, "vim-enhanced");
        assert_eq!(hits[1].meta.repo.as_deref(), Some("updates"));
        assert_eq!(hits[2].name, "empty-summary");
        assert_eq!(hits[2].description, "");
        assert!(parse_catalog("").is_empty());
    }

    #[test]
    fn filters_catalog_by_name_or_summary_case_insensitive() {
        let catalog = parse_catalog(
            "ripgrep\t14\tfedora\tLine-oriented search tool\n\
             the-silver-searcher\t2\tfedora\tA code searching tool like ack\n\
             foo\t1\tfedora\tbar\n",
        );
        // name match, case-insensitive
        let hits = filter_catalog(&catalog, "RIPGREP");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "ripgrep");
        // summary-only match
        let hits = filter_catalog(&catalog, "ack");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "the-silver-searcher");
        // name OR summary: "search" is in ripgrep's summary and silver-searcher's name
        let hits = filter_catalog(&catalog, "search");
        let names: Vec<String> = hits.iter().map(|h| h.name.clone()).collect();
        assert_eq!(names, vec!["ripgrep", "the-silver-searcher"]);
        assert!(filter_catalog(&catalog, "zzz").is_empty());
    }

    #[test]
    fn parses_info_into_detail() {
        let out = "\
Name         : vim-enhanced
Epoch        : 2
Version      : 9.1.158
Release      : 1.fc41
Architecture : x86_64
Installed size: 4.5 MiB
Repository   : updates
Summary      : A version of the VIM editor
URL          : http://www.vim.org/
License      : Vim AND LGPL-2.1-or-later
Description  : VIM (VIsual editor iMproved) is an updated
             : and improved version of the vi editor.
";
        let d = parse_info_output(out);
        assert_eq!(d.url.as_deref(), Some("http://www.vim.org/"));
        assert_eq!(d.licenses.as_deref(), Some("Vim AND LGPL-2.1-or-later"));
        assert_eq!(d.install_size.as_deref(), Some("4.5 MiB"));
        assert!(d.build_date.is_none());
        assert!(d.depends.is_empty());
    }

    #[test]
    fn parses_requires_dedups_and_trims() {
        let out = "glibc\nvim-common\nglibc\nlibgpm.so.2()(64bit)\n\n";
        let deps = parse_requires(out);
        assert_eq!(deps, vec!["glibc", "vim-common", "libgpm.so.2"]);
    }
}
