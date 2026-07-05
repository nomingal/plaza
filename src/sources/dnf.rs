use crate::model::{Action, CommandLine, PackageDetail, PackageHit, SourceId, SourceMeta};
use crate::sources::Source;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::process::Command;

pub struct DnfSource;

impl DnfSource {
    pub fn new() -> Self {
        DnfSource
    }
}

#[async_trait]
impl Source for DnfSource {
    fn id(&self) -> SourceId {
        SourceId::Dnf
    }

    fn display_name(&self) -> &'static str {
        "dnf"
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<PackageHit>> {
        // `dnf search` matches name + summary but gives no version or repo; a
        // batched `dnf repoquery` over the matched names fills those in.
        let search_out =
            Command::new("dnf").env("LC_ALL", "C").arg("search").arg(query).output().await?;
        let search = String::from_utf8_lossy(&search_out.stdout).into_owned();
        let names: Vec<String> =
            parse_search_output(&search).into_iter().map(|(n, _)| n).collect();
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let repoquery_out = Command::new("dnf")
            .env("LC_ALL", "C")
            .args(["repoquery", "--qf", "%{name}\t%{evr}\t%{reponame}\n", "--latest-limit=1"])
            .args(&names)
            .output()
            .await?;
        let repoquery = String::from_utf8_lossy(&repoquery_out.stdout);
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

/// Parse `dnf search <q>` output into `(name, summary)` pairs. Package lines are
/// `name.arch: summary` (dnf5) or `name.arch : summary` (dnf4); section headers
/// like `Matched fields: name` or `=== ... Matched: ===` have whitespace in the
/// part before the colon (a package `name.arch` never does) and are skipped.
pub fn parse_search_output(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (left, summary) = line.split_once(':')?;
            let left = left.trim();
            // Headers ("Matched fields", "Name Exactly Matched") carry spaces;
            // a package token is a single `name.arch`.
            if left.is_empty() || left.contains(char::is_whitespace) || !left.contains('.') {
                return None;
            }
            Some((strip_arch(left).to_string(), summary.trim().to_string()))
        })
        .collect()
}

/// Parse `dnf repoquery --qf '%{name}\t%{evr}\t%{reponame}\n'` into a map of
/// name -> (evr, reponame). The first line for a name wins (repoquery is run
/// with `--latest-limit=1`, so that is the newest candidate).
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
/// PackageHits. A searched name with no repoquery candidate is dropped.
pub fn build_hits(search: &str, repoquery: &str) -> Vec<PackageHit> {
    let rq = parse_repoquery(repoquery);
    parse_search_output(search)
        .into_iter()
        .filter_map(|(name, description)| {
            let (version, repo) = rq.get(&name)?.clone();
            Some(PackageHit {
                name,
                version,
                source_id: SourceId::Dnf,
                description,
                meta: SourceMeta { repo: Some(repo), maintained: true, ..Default::default() },
            })
        })
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

/// Parse `dnf repoquery --requires --resolve --qf '%{name}\n'` (one dependency
/// package name per line) into a deduped list. Any version constraint or file
/// path fragment after the bare name token is dropped; blank lines are ignored.
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
        let out = "\
Matched fields: name
vim-minimal.x86_64: A minimal version of the VIM editor
vim-enhanced.x86_64: A version of the VIM editor which includes: recent enhancements
Matched fields: name, summary
protobuf-vim.noarch : Vim syntax highlighting
";
        let hits = parse_search_output(out);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0], ("vim-minimal".to_string(), "A minimal version of the VIM editor".to_string()));
        // summary may itself contain a colon; only the first colon splits
        assert_eq!(hits[1].0, "vim-enhanced");
        assert_eq!(hits[1].1, "A version of the VIM editor which includes: recent enhancements");
        // dnf4-style "name.arch : summary" also parses, arch stripped
        assert_eq!(hits[2].0, "protobuf-vim");
        assert!(parse_search_output("").is_empty());
    }

    #[test]
    fn parses_repoquery_first_per_name() {
        let out = "vim-enhanced\t2:9.1.158-1.fc41\tupdates\nvim-enhanced\t2:9.1.100-1.fc41\tfedora\n";
        let m = parse_repoquery(out);
        assert_eq!(m.get("vim-enhanced").unwrap(), &("2:9.1.158-1.fc41".to_string(), "updates".to_string()));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn build_hits_joins_and_drops_unmatched() {
        let search = "vim-enhanced.x86_64: Enhanced vi\norphan.noarch: not in repoquery\n";
        let repoquery = "vim-enhanced\t2:9.1.158-1.fc41\tupdates\n";
        let hits = build_hits(search, repoquery);
        assert_eq!(hits.len(), 1); // orphan without a repoquery candidate is dropped
        let h = &hits[0];
        assert_eq!(h.name, "vim-enhanced");
        assert_eq!(h.version, "2:9.1.158-1.fc41");
        assert_eq!(h.source_id, SourceId::Dnf);
        assert_eq!(h.description, "Enhanced vi");
        assert_eq!(h.meta.repo.as_deref(), Some("updates"));
        assert!(h.meta.maintained);
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
