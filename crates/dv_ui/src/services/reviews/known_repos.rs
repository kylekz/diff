//! The repositories dv knows (PLAN §11 `Reviews`): `<data dir>/known_repos.json`,
//! `{"v": 1, "repos": [{"location", "added_at", "opened_at", "github"}]}`, times in ms since
//! the Unix epoch; `github` is the repository on GitHub its clone is, `{"host", "owner",
//! "repo"}` as last read (PLAN §9.1), absent when it has none or was never read. The palette
//! matches a pasted pull request against it without opening the repository (PLAN §9.9).
//!
//! Deliberately not the first implementation's `repos.json`, which sits in the same default
//! data dir with another shape. Read leniently ([`super::super::persist`]): unknown keys, at the
//! top and in each entry, are kept and written back; an entry this version can't read (no
//! location, another version's shape) is kept as it was, after a warning; a repeated location
//! counts once, a WSL location's distro compared without case (`Location::same_place`: `wsl.exe`
//! takes distro names so).

use dv_core::Location;
use dv_core::github::RepoUrl;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// The file in the data dir.
pub const KNOWN_REPOS_FILE: &str = "known_repos.json";
/// The `"v"` this version writes.
pub const KNOWN_REPOS_VERSION: u64 = 1;

/// One known repository.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnownRepo {
    /// The worktree root, as `Location` serialises (`{"local": "D:\\code\\api"}`).
    pub location: Location,
    /// When dv first saw it.
    #[serde(default)]
    pub added_at: u64,
    /// When a review of it was last opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<u64>,
    /// Its repository on GitHub, as last read beside its listing.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_github"
    )]
    pub github: Option<KnownGithub>,
    /// Keys this version doesn't know.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl KnownRepo {
    pub fn new(location: Location, now: u64) -> Self {
        Self {
            location,
            added_at: now,
            opened_at: None,
            github: None,
            extra: Map::new(),
        }
    }

    /// When it was last used: opened, else added. Lists show the most recent first.
    pub fn last_used(&self) -> u64 {
        self.opened_at.unwrap_or(self.added_at).max(self.added_at)
    }
}

/// A known repository's `github`: the repository's name, and the keys this version doesn't
/// know, kept and written back as an entry's are.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnownGithub {
    #[serde(flatten)]
    pub repo: RepoUrl,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl std::ops::Deref for KnownGithub {
    type Target = RepoUrl;

    fn deref(&self) -> &RepoUrl {
        &self.repo
    }
}

/// An entry's `github`, read leniently: anything that isn't a host, an owner and a
/// repository's name as GitHub has them (another shape, a name no repository has, a file
/// edited by hand) reads as none, and the next listing writes what the clone says.
fn lenient_github<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<KnownGithub>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<KnownGithub>(value)
        .ok()
        .filter(|github| github.is_well_formed()))
}

/// An entry of the file, as read.
#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Known(Box<KnownRepo>),
    /// An entry this version can't read, written back as it was.
    Kept(Value),
}

/// The file's contents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KnownReposFile {
    entries: Vec<Entry>,
    /// Top-level keys this version doesn't know.
    extra: Map<String, Value>,
}

impl KnownReposFile {
    /// Reads the file's object leniently: the repos, and a line for each thing that couldn't
    /// be read. `Err` when `repos` isn't a list at all: not a file this version can read, so
    /// it is never written over.
    pub fn from_object(mut object: Map<String, Value>) -> Result<(Self, Vec<String>), String> {
        let mut problems = Vec::new();
        if let Some(v) = object.get("v") {
            match v.as_u64() {
                Some(v) if v > KNOWN_REPOS_VERSION => problems.push(format!(
                    "written by a newer dv (v {v}); reading the entries this version knows"
                )),
                Some(_) => {}
                None => problems.push(format!("\"v\" is {v}, not a version number")),
            }
        }
        let entries = match object.remove("repos") {
            Some(Value::Array(entries)) => entries,
            None => Vec::new(),
            Some(other) => return Err(format!("\"repos\" is {other}, not a list")),
        };
        object.remove("v");
        let mut file = Self::default();
        for (index, value) in entries.into_iter().enumerate() {
            match serde_json::from_value::<KnownRepo>(value.clone()) {
                Ok(repo) if file.find_place(&repo.location).is_none() => {
                    file.entries.push(Entry::Known(Box::new(repo)))
                }
                Ok(repo) => problems.push(format!(
                    "repos[{index}] repeats {}; reading it once",
                    repo.location
                )),
                Err(error) => {
                    problems.push(format!("repos[{index}] can't be read ({error}); kept"));
                    file.entries.push(Entry::Kept(value));
                }
            }
        }
        file.extra = object;
        Ok((file, problems))
    }

    /// The repos this version can read, in file order.
    pub fn repos(&self) -> impl Iterator<Item = &KnownRepo> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Known(repo) => Some(repo.as_ref()),
            Entry::Kept(_) => None,
        })
    }

    pub fn get(&self, location: &Location) -> Option<&KnownRepo> {
        self.repos().find(|repo| &repo.location == location)
    }

    /// The known repo at the place `location` names, however its WSL distro is spelled
    /// (`Location::same_place`).
    pub fn find_place(&self, location: &Location) -> Option<&KnownRepo> {
        self.repos().find(|repo| repo.location.same_place(location))
    }

    fn get_mut(&mut self, location: &Location) -> Option<&mut KnownRepo> {
        self.entries.iter_mut().find_map(|entry| match entry {
            Entry::Known(repo) if &repo.location == location => Some(repo.as_mut()),
            _ => None,
        })
    }

    /// Adds `location` (added now) unless it is known, however its WSL distro is spelled
    /// ([`KnownReposFile::find_place`]); `true` when it was added.
    pub fn add(&mut self, location: Location, now: u64) -> bool {
        if self.find_place(&location).is_some() {
            return false;
        }
        self.entries
            .push(Entry::Known(Box::new(KnownRepo::new(location, now))));
        true
    }

    /// Records that a review of `location` was opened now; `false` when it isn't known.
    pub fn mark_opened(&mut self, location: &Location, now: u64) -> bool {
        match self.get_mut(location) {
            Some(repo) => {
                repo.opened_at = Some(now);
                true
            }
            None => false,
        }
    }

    /// Remembers `github` as the repository on GitHub `location`'s clone is (`None`: it has
    /// none); `true` when that changed what is remembered, a name's spelling included.
    pub fn set_github(&mut self, location: &Location, github: Option<RepoUrl>) -> bool {
        let Some(repo) = self.get_mut(location) else {
            return false;
        };
        let same = match (&repo.github, &github) {
            (Some(known), Some(read)) => {
                known.host == read.host && known.owner == read.owner && known.repo.repo == read.repo
            }
            (None, None) => true,
            _ => false,
        };
        if !same {
            // The keys this version doesn't know stay with the entry's `github`.
            let extra = repo
                .github
                .take()
                .map(|known| known.extra)
                .unwrap_or_default();
            repo.github = github.map(|repo| KnownGithub { repo, extra });
        }
        !same
    }

    /// Knows `old` as `new` from now on (the worktree root it resolved to, or WSL's spelling of
    /// its distro), its times kept; when `new` is known already (however spelled), `old` goes,
    /// the later of the two opens kept. `true` when anything changed.
    pub fn rekey(&mut self, old: &Location, new: &Location) -> bool {
        if old == new || self.get(old).is_none() {
            return false;
        }
        let known = self
            .repos()
            .find(|repo| &repo.location != old && repo.location.same_place(new))
            .map(|repo| repo.location.clone());
        match known {
            Some(known) => {
                let opened = self.get(old).and_then(|repo| repo.opened_at);
                self.remove(old);
                if let Some(repo) = self.get_mut(&known) {
                    repo.opened_at = repo.opened_at.max(opened);
                    if &known != new {
                        repo.location = new.clone();
                    }
                }
            }
            None => {
                if let Some(repo) = self.get_mut(old) {
                    repo.location = new.clone();
                }
            }
        }
        true
    }

    /// Forgets `location`; `true` when it was known.
    pub fn remove(&mut self, location: &Location) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !matches!(entry, Entry::Known(repo) if &repo.location == location));
        self.entries.len() != before
    }

    /// The file's text: `v` and `repos` first, then the kept keys; pretty JSON with a trailing
    /// newline.
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("v".into(), KNOWN_REPOS_VERSION.into());
        let repos: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Known(repo) => serde_json::to_value(repo)
                    .unwrap_or_else(|error| unreachable!("a known repo serialises: {error}")),
                Entry::Kept(value) => value.clone(),
            })
            .collect();
        object.insert("repos".into(), Value::Array(repos));
        for (key, value) in &self.extra {
            object.insert(key.clone(), value.clone());
        }
        let mut text = serde_json::to_string_pretty(&Value::Object(object))
            .unwrap_or_else(|error| unreachable!("JSON values serialise: {error}"));
        text.push('\n');
        text
    }
}
