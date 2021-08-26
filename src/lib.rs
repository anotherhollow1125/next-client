use anyhow::{Context, Result};
#[allow(unused_imports)]
use log::{debug, info};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
// use notify::DebouncedEvent;

#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate if_chain;
#[macro_use]
extern crate async_recursion;

pub mod errors;
mod fileope;
pub mod local_listen;
pub mod meta;
pub mod nc_listen;
pub mod network;
pub mod repair;

pub struct PublicResource {
    pub root: ArcEntry,
    pub nc_state: nc_listen::NCState,
    // pub local_event_que: VecDeque<notify::DebouncedEvent>,
}

pub type ArcResource = Arc<Mutex<PublicResource>>;
pub type WeakResource = Weak<Mutex<PublicResource>>;

impl PublicResource {
    pub fn new(root: ArcEntry, nc_state: nc_listen::NCState) -> Self {
        Self {
            root,
            nc_state,
            // local_event_que: VecDeque::new(),
        }
    }
}

use errors::NcsError::*;

pub static RE_HAS_LAST_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("(.*)/$").unwrap());
pub static RE_HAS_HEAD_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("^/(.*)").unwrap());

pub fn add_head_slash(s: &str) -> String {
    if RE_HAS_HEAD_SLASH.is_match(s) {
        s.to_string()
    } else {
        format!("/{}", s)
    }
}

pub fn add_last_slash(s: &str) -> String {
    if RE_HAS_LAST_SLASH.is_match(s) {
        s.to_string()
    } else {
        format!("{}/", s)
    }
}

pub fn drop_slash(s: &str, re: &Regex) -> String {
    if re.is_match(s) {
        re.replace(s, "$1").to_string()
    } else {
        s.to_string()
    }
}

pub fn fix_host(host: &str) -> String {
    drop_slash(host, &RE_HAS_LAST_SLASH)
}

pub fn fix_root(root_path: &str) -> String {
    let root_path = drop_slash(root_path, &RE_HAS_LAST_SLASH);

    let root_path = if !RE_HAS_HEAD_SLASH.is_match(&root_path) {
        format!("/{}", root_path)
    } else {
        root_path
    };

    root_path
}

pub fn path2name(path: &str) -> String {
    let p = drop_slash(path, &RE_HAS_LAST_SLASH);
    p.split("/").last().unwrap_or("").to_string()
}

// To be honest, the naming failed
pub fn path2str(path: &Path) -> String {
    let path = path.to_string_lossy().to_string();
    let path = path.replace("\\", "/");
    let path = add_head_slash(&path);
    drop_slash(&path, &RE_HAS_LAST_SLASH)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryType {
    File { etag: Option<String> },
    Directory,
}

impl EntryType {
    pub fn is_file(&self) -> bool {
        match self {
            &Self::File { .. } => true,
            _ => false,
        }
    }

    pub fn is_dir(&self) -> bool {
        !self.is_file()
    }

    pub fn get_etag(&self) -> String {
        match self {
            &Self::Directory => "".to_string(),
            &Self::File { ref etag } => etag.as_ref().map(|s| s.as_str()).unwrap_or("").to_string(),
        }
    }

    pub fn is_same_type(&self, other: &Self) -> bool {
        if self.is_file() {
            other.is_file()
        } else {
            other.is_dir()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    UpToDate,
    NeedUpdate,
    Error,
}

impl fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpToDate => write!(f, ""),
            Self::NeedUpdate => write!(f, "*"),
            Self::Error => write!(f, "!"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    name: String,
    parent: Option<WeakEntry>,
    pub status: EntryStatus,
    pub type_: EntryType,
    children: HashMap<String, ArcEntry>,
}

pub type ArcEntry = Arc<Mutex<Entry>>;
pub type WeakEntry = Weak<Mutex<Entry>>;

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let EntryType::File { ref etag } = self.type_ {
            write!(f, "{}{} etag: {:?}", self.status, self.get_name(), etag)
        } else {
            write!(f, "{}{}", self.status, self.get_name())
        }
    }
}

impl Entry {
    pub fn new(name: String, type_: EntryType) -> Self {
        let name = drop_slash(&name, &RE_HAS_LAST_SLASH);
        Entry {
            name,
            type_,
            status: EntryStatus::NeedUpdate,
            parent: None,
            children: HashMap::new(),
        }
    }

    pub fn get_name(&self) -> String {
        let s = match &self.type_ {
            &EntryType::Directory => "/",
            _ => "",
        };

        format!("{}{}", self.name, s)
    }

    pub fn get_raw_name(&self) -> String {
        self.name.clone()
    }

    pub fn set_name(&mut self, new_name: &str) {
        let new_name = drop_slash(new_name, &RE_HAS_LAST_SLASH);
        self.name = new_name;
    }

    pub fn is_root(&self) -> bool {
        self.name == "" && self.parent.is_none()
    }

    pub fn get_parent(entry: &ArcEntry) -> Result<Option<ArcEntry>> {
        let entry = entry.lock().map_err(|_| LockError)?;
        let p = match entry.parent.as_ref() {
            Some(p) => p,
            _ => return Ok(None),
        };
        Ok(Some(p.upgrade().with_context(|| WeakUpgradeError)?))
    }

    // only can be used when it's ancestors are not locked.
    // for example, you can't use it in recursive context.
    pub fn get_path(entry: &ArcEntry) -> Result<String> {
        let mut parent_names = Vec::new();
        let mut p = entry.clone();
        while let Some(q) = Entry::get_parent(&p)? {
            let name = { q.lock().map_err(|_| LockError)?.get_name() };
            parent_names.push(name);
            p = q;
        }
        parent_names.reverse();
        let entry = entry.lock().map_err(|_| LockError)?;
        Ok(format!("{}{}", parent_names.join(""), entry.get_raw_name()))
    }

    pub fn append_child(parent: &ArcEntry, child: ArcEntry) -> Result<()> {
        let weak_parent = Arc::downgrade(parent);
        let child_name = {
            let mut child_ref = child.lock().map_err(|_| LockError)?;
            child_ref.parent = Some(weak_parent);
            child_ref.get_raw_name()
        };
        parent
            .lock()
            .map_err(|_| LockError)?
            .children
            .insert(child_name, child);
        Ok(())
    }

    pub fn get_child(&self, child_name: &str) -> Option<WeakEntry> {
        const RE_LAST_ITEM: Lazy<Regex> = Lazy::new(|| Regex::new(r".*?([^/\\]*)$").unwrap());

        let child_name = RE_LAST_ITEM.replace(child_name, "$1").to_string();
        self.children.get(&child_name).map(|a| Arc::downgrade(a))
    }

    pub fn get_all_children(&self) -> Vec<WeakEntry> {
        self.children
            .values()
            .map(|c| Arc::downgrade(c))
            .collect::<Vec<_>>()
    }

    pub fn get_tree(&self) -> String {
        let mut res = String::new();

        self.tree_rec(&mut res, "");

        res
    }

    fn tree_rec(&self, tree: &mut String, indent: &str) {
        let s = format!("{}\n", self);
        tree.push_str(s.as_str());
        let mut ch_iter = self.children.values().peekable();
        while let Some(c) = ch_iter.next() {
            if let Ok(c) = c.lock() {
                let is_last = ch_iter.peek().is_some();
                tree.push_str(
                    format!("{}{}", indent, if is_last { "├── " } else { "└── " }).as_str(),
                );
                c.tree_rec(
                    tree,
                    format!("{}{}   ", indent, if is_last { "|" } else { " " }).as_str(),
                );
            }
        }
    }

    pub fn prepare_path_vec(path: &str) -> Vec<String> {
        let path = drop_slash(path, &RE_HAS_LAST_SLASH);
        let mut path_vec = path
            .split("/")
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        if path_vec.len() == 0 {
            path_vec.push("".to_string());
        }
        path_vec.reverse();

        path_vec
    }

    pub fn get(this: &ArcEntry, path: &str) -> Result<Option<WeakEntry>> {
        let root_w = Arc::downgrade(this);
        let root_ref = this.lock().map_err(|_| LockError)?;
        if !root_ref.is_root() {
            return Ok(None);
        }

        let path_vec = Self::prepare_path_vec(path);

        if path_vec.len() == 0 {
            return Ok(None);
        }

        if path_vec.len() == 1 {
            if path_vec[0] == "" {
                return Ok(Some(root_w));
            } else {
                return Ok(None);
            }
        }

        root_ref.get_rec(path_vec)
    }

    fn get_rec(&self, mut path_vec: Vec<String>) -> Result<Option<WeakEntry>> {
        let name_w = path_vec.pop();
        if name_w.is_none() || self.get_raw_name() != name_w.unwrap() {
            return Ok(None);
        }

        if self.type_.is_file() {
            return Err(anyhow!("This function can only be called by directory."));
        }

        let len = path_vec.len();
        if let Some(e) = self.children.get(&path_vec[len - 1]) {
            let e_weak = Arc::downgrade(e);
            let e = e.lock().map_err(|_| LockError)?;
            if e.type_.is_file() {
                Ok(Some(e_weak))
            } else {
                // When the path specify directory
                if len == 1 {
                    Ok(Some(e_weak))
                } else {
                    e.get_rec(path_vec)
                }
            }
        } else {
            Ok(None)
        }
    }

    pub fn pop(&mut self, path: &str) -> Result<Option<ArcEntry>> {
        if !self.is_root() {
            return Ok(None);
        }

        let path_vec = Self::prepare_path_vec(path);

        self.pop_rec(path_vec)
    }

    fn pop_rec(&mut self, mut path_vec: Vec<String>) -> Result<Option<ArcEntry>> {
        let name_w = path_vec.pop();
        if name_w.is_none() || self.get_raw_name() != name_w.unwrap() {
            return Ok(None);
        }

        if self.type_.is_file() {
            return Err(anyhow!("This function can only be called by directory."));
        }

        let len = path_vec.len();
        if len > 1 {
            // continue phase
            if let Some(e) = self.children.get(&path_vec[len - 1]) {
                let mut e = e.lock().map_err(|_| LockError)?;
                if e.type_.is_file() {
                    Ok(None)
                } else {
                    e.pop_rec(path_vec)
                }
            } else {
                Ok(None)
            }
        } else {
            // pop target
            let entry = self.children.remove(&path_vec[0]);

            if let Some(entry) = entry {
                {
                    let mut entry_ref = entry.lock().map_err(|_| LockError)?;
                    entry_ref.parent = None;
                }
                Ok(Some(entry))
            } else {
                Ok(None)
            }
        }
    }

    pub fn append(
        root_entry: &ArcEntry,
        path: &str,
        entry: ArcEntry,
        append_mode: AppendMode,
        overwrite: bool,
    ) -> Result<Vec<WeakEntry>> {
        {
            let root_ref = root_entry.lock().map_err(|_| LockError)?;
            if !root_ref.is_root() {
                return Ok(Vec::new());
            }
        }

        let target_parent_cand =
            Entry::get(root_entry, drop_slash(path, &RE_HAS_LAST_SLASH).as_str())?;

        if !overwrite && target_parent_cand.is_some() {
            return Err(anyhow!("Already Exist!!"));
        }

        let mut path_vec = Self::prepare_path_vec(path);

        {
            let mut entry_ref = entry.lock().map_err(|_| LockError)?;
            let entry_name = entry_ref.get_raw_name();
            if &path_vec[0] != &entry_name {
                match append_mode {
                    AppendMode::Move => {
                        if_chain! {
                            if let Some(w) = target_parent_cand;
                            if let Some(e) = w.upgrade();
                            let e_ref = e.lock().map_err(|_| LockError)?;
                            if e_ref.type_.is_dir();
                            then {
                                path_vec.insert(0, entry_name);
                            } else {
                                entry_ref.set_name(&path_vec[0]);
                            }
                        }
                    }
                    AppendMode::Create => return Err(InvalidPathError(path.to_string()).into()),
                }
            }
        }

        let mut new_local_entries = Vec::new();
        Self::append_rec(
            root_entry,
            path_vec,
            entry,
            &mut new_local_entries,
            append_mode,
        )?;

        Ok(new_local_entries)
    }

    fn append_rec(
        parent_entry: &ArcEntry,
        mut path_vec: Vec<String>,
        entry: ArcEntry,
        new_local_entries: &mut Vec<WeakEntry>,
        append_mode: AppendMode,
    ) -> Result<()> {
        let name_w = path_vec.pop();
        {
            let parent_ref = parent_entry.lock().map_err(|_| LockError)?;
            let parent_name = parent_ref.get_raw_name();
            if name_w.is_none() || &parent_name != name_w.unwrap().as_str() {
                return Err(InvalidPathError(format!("{}", parent_name)).into());
            }
            if parent_ref.type_.is_file() {
                return Err(anyhow!("This function can only be called by directory."));
            }
        }

        let len = path_vec.len();
        if len > 1 {
            // continue phase
            let name = path_vec[len - 1].to_string();
            let e_w = {
                let parent_ref = parent_entry.lock().map_err(|_| LockError)?;
                parent_ref.children.get(&name).map(|e| e.clone())
            };
            let e = if let Some(e) = e_w {
                e
            } else {
                let new_entry = Arc::new(Mutex::new(Self::new(name, EntryType::Directory)));
                new_local_entries.push(Arc::downgrade(&new_entry));
                Self::append_child(parent_entry, new_entry.clone())?;
                new_entry
            };

            let (is_file, e_name) = {
                let e_ref = e.lock().map_err(|_| LockError)?;
                (e_ref.type_.is_file(), e_ref.get_name())
            };
            if is_file {
                Err(InvalidPathError(e_name).into())
            } else {
                Self::append_rec(&e, path_vec, entry, new_local_entries, append_mode)
            }
        } else {
            // append target
            if append_mode == AppendMode::Create {
                new_local_entries.push(Arc::downgrade(&entry));
            }
            Self::append_child(parent_entry, entry)?;

            Ok(())
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppendMode {
    Create,
    Move,
}

#[derive(Debug)]
pub enum Command {
    NCEvents(Vec<nc_listen::NCEvent>, nc_listen::NCState),
    LocEvent(local_listen::LocalEvent),
    UpdateExcFile,
    UpdateConfigFile,
    HardRepair,
    NormalRepair,
    NetworkConnect,
    NetworkDisconnect,
    Terminate(bool),
    Error(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn path_vec_test1() {
        let path = "/hoge/fuga/bar/test.md";

        let v = Entry::prepare_path_vec(path);
        assert_eq!(&v, &["test.md", "bar", "fuga", "hoge", ""]);

        let path = "/hoge/fuga/bar/test/";

        let v = Entry::prepare_path_vec(path);
        assert_eq!(&v, &["test", "bar", "fuga", "hoge", ""]);

        let path = "/";

        let v = Entry::prepare_path_vec(path);
        assert_eq!(&v, &[""]);
    }
}
