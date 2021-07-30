use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate if_chain;
#[macro_use]
extern crate async_recursion;

pub mod dump;
pub mod errors;
mod fileope;
pub mod nc_listen;

use errors::NcsError::*;

pub static RE_HAS_LAST_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("(.*)/$").unwrap());
pub static RE_HAS_HEAD_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("^/(.*)").unwrap());

pub struct LocalInfo {
    pub root_path: String,
}

impl LocalInfo {
    pub fn new(root_path: String) -> Self {
        let root_path = drop_slash(&root_path, &RE_HAS_LAST_SLASH);
        Self { root_path }
    }

    pub fn get_metadir_name(&self) -> String {
        format!("{}/.ncs/", self.root_path)
    }

    pub fn get_cachefile_name(&self) -> String {
        format!("{}cache.json", self.get_metadir_name())
    }
}

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
    let raw_name = p.split("/").last().unwrap_or("").to_string();

    if RE_HAS_LAST_SLASH.is_match(path) {
        format!("{}/", raw_name)
    } else {
        raw_name
    }
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

    pub fn guess_from_name(name: &str) -> Self {
        if RE_HAS_LAST_SLASH.is_match(name) {
            Self::Directory
        } else {
            Self::File { etag: None }
        }
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

/*
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        if_chain! {
            if let EntryType::File { etag: ref self_etag } = self.type_;
            if let EntryType::File { etag: ref other_etag } = other.type_;
            then {
                if_chain! {
                    if let Some(self_etag) = self_etag;
                    if let Some(other_etag) = other_etag;
                    then {
                        self.path_eq(other).unwrap() && self_etag == other_etag
                    } else {
                        false
                    }
                }
            } else {
                self.path_eq(other).unwrap()
            }
        }
    }
}

impl Eq for Entry {}
*/

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let EntryType::File { ref etag } = self.type_ {
            write!(f, "{}{} etag: {:?}", self.status, self.get_name(), etag)
        } else {
            write!(f, "{}{}", self.status, self.get_name())
        }
    }
}

// static RE_GET_PARENT: Lazy<Regex> = Lazy::new(|| Regex::new("(.*/).*?$").unwrap());

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

    pub fn set_name(&mut self, new_name: &str) {
        let new_name = drop_slash(new_name, &RE_HAS_LAST_SLASH);
        self.name = new_name;
    }

    /*
    pub fn path_eq(&self, other: &Self) -> Result<bool> {
        let self_path = self.get_path()?;
        let other_path = other.get_path()?;

        Ok(self_path == other_path)
    }
    */

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
        Ok(format!("{}{}", parent_names.join(""), entry.get_name()))
    }

    pub fn append_child(parent: &ArcEntry, child: ArcEntry) -> Result<()> {
        let weak_parent = Arc::downgrade(parent);
        let child_name = {
            let mut child_ref = child.lock().map_err(|_| LockError)?;
            child_ref.parent = Some(weak_parent);
            child_ref.get_name()
        };
        parent
            .lock()
            .map_err(|_| LockError)?
            .children
            .insert(child_name, child);
        Ok(())
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

    fn prepare_path_vec(path: &str) -> Vec<String> {
        let v = path.split("/").enumerate().collect::<Vec<_>>();
        let len = v.len();
        let mut path_vec = Vec::with_capacity(len);

        for (i, name) in v.into_iter() {
            let name = if i < len - 1 {
                format!("{}/", name)
            } else {
                name.to_string()
            };
            path_vec.push(name);
        }
        path_vec.reverse();

        if &path_vec[0] == "" {
            (&path_vec[1..]).iter().map(|s| s.to_string()).collect()
        } else {
            path_vec
        }
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
            if path_vec[0] == "/" {
                return Ok(Some(root_w));
            } else {
                return Ok(None);
            }
        }

        root_ref.get_rec(path_vec)
    }

    fn get_rec(&self, mut path_vec: Vec<String>) -> Result<Option<WeakEntry>> {
        let name_w = path_vec.pop();
        if name_w.is_none() || self.get_name() != name_w.unwrap() {
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
        if name_w.is_none() || self.get_name() != name_w.unwrap() {
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
            let entry = self.children.remove(&path_vec[len - 1]);

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
    ) -> Result<Vec<(WeakEntry, EntryStatus)>> {
        {
            let root_ref = root_entry.lock().map_err(|_| LockError)?;
            if !root_ref.is_root() {
                return Ok(Vec::new());
            }
        }

        let already_exist = Entry::get(root_entry, add_last_slash(path).as_str())?.is_some()
            || Entry::get(root_entry, path)?.is_some();

        if !overwrite && already_exist {
            return Err(anyhow!("Already Exist!!"));
        }

        let mut path_vec = Self::prepare_path_vec(path);

        {
            let mut entry_ref = entry.lock().map_err(|_| LockError)?;
            let entry_name = entry_ref.get_name();
            if &path_vec[0] != &entry_name {
                match append_mode {
                    AppendMode::Move => {
                        if_chain! {
                            // if entry_ref.type_.is_file();
                            if already_exist;
                            if EntryType::guess_from_name(path).is_dir();
                            /*
                            if let root_ref = root_entry.lock().map_err(|_| LockError)?;
                            if let Some(w) = root_ref.get(format!("{}/", path).as_str())?;
                            if let Some(e) = w.upgrade();
                            if let e_ref = e.lock().map_err(|_| LockError)?;
                            if e_ref.type_.is_dir();
                            */
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
        new_local_entries: &mut Vec<(WeakEntry, EntryStatus)>,
        append_mode: AppendMode,
    ) -> Result<()> {
        let name_w = path_vec.pop();
        {
            let parent_ref = parent_entry.lock().map_err(|_| LockError)?;
            let parent_name = parent_ref.get_name();
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
                new_local_entries.push((Arc::downgrade(&new_entry), EntryStatus::UpToDate));
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
            {
                let mut entry_ref = entry.lock().map_err(|_| LockError)?;
                entry_ref.status = EntryStatus::NeedUpdate;
            }
            if append_mode == AppendMode::Create {
                new_local_entries.push((Arc::downgrade(&entry), EntryStatus::NeedUpdate));
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
