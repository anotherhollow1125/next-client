use once_cell::sync::Lazy;
use regex::Regex;
// use std::cell::RefCell;
use std::fmt;
// use std::rc::{Rc, Weak};
use std::collections::{HashMap, HashSet};

#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate if_chain;

mod fileope;
pub mod nc_listen;

pub static RE_HAS_LAST_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("(.*)/$").unwrap());
pub static RE_HAS_HEAD_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("^/(.*)").unwrap());

pub struct LocalInfo {
    root_path: String,
}

impl LocalInfo {
    pub fn new(root_path: String) -> Self {
        let root_path = drop_slash(&root_path, &RE_HAS_LAST_SLASH);
        Self { root_path }
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

/*
type RcEntry = Rc<RefCell<Entry>>;
type WeakEntry = Weak<RefCell<Entry>>;
*/

#[derive(Debug, Clone)]
pub enum EntryType {
    File { etag: Option<String> },
    Directory { children: HashSet<String> },
    // Directory { children: Vec<WeakEntry> },
}

impl EntryType {
    pub fn is_file(&self) -> bool {
        match self {
            &Self::File { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    UpToDate,
    // NeedUpdateEtag,
    NeedUpdate,
}

impl fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpToDate => write!(f, ""),
            Self::NeedUpdate => write!(f, "*"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: String,
    // pub etag: String,
    pub status: EntryStatus,
    pub type_: EntryType,
}

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
                        self.path_eq(other) && self_etag == other_etag
                    } else {
                        false
                    }
                }
            } else {
                self.path_eq(other)
            }
        }
    }
}

impl Eq for Entry {}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /*
        let etag_len = if self.etag.len() >= 8 {
            8
        } else {
            self.etag.len()
        };
        write!(f, "{} ({})", self.name, &self.etag[..etag_len])
        */
        if let EntryType::File { ref etag } = self.type_ {
            write!(f, "{}{} etag: {:?}", self.status, self.name, etag)
        } else {
            write!(f, "{}{}", self.status, self.name)
        }
    }
}

static RE_GET_PARENT: Lazy<Regex> = Lazy::new(|| Regex::new("(.*/).*?$").unwrap());

impl Entry {
    pub fn from_path(raw_path: &str, type_: EntryType) -> Self {
        let path = add_head_slash(raw_path);
        let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
        let name = path.split("/").last().unwrap_or("").to_string();
        let (name, path) = match &type_ {
            &EntryType::File { .. } => (name, path),
            _ => (add_last_slash(&name), add_last_slash(&path)),
        };

        let status = EntryStatus::NeedUpdate;

        Entry {
            path,
            name,
            status,
            type_,
        }
    }

    fn path_eq(&self, other: &Self) -> bool {
        self.name == other.name
    }

    fn get_parent_name(&self) -> Option<String> {
        if self.path == "/" {
            return None;
        }

        let p = drop_slash(&self.path, &RE_HAS_LAST_SLASH);
        Some(RE_GET_PARENT.replace(&p, "$1").to_string())
    }

    pub fn get_tree(&self, book: &HashMap<String, Self>) -> String {
        let mut res = String::new();

        self.tree_rec(book, &mut res, "");

        res
    }

    fn tree_rec(&self, book: &HashMap<String, Self>, tree: &mut String, indent: &str) {
        match self.type_ {
            EntryType::File { .. } => {
                let s = format!("{}\n", self);
                tree.push_str(s.as_str());
            }
            EntryType::Directory { ref children } => {
                let s = format!("{}\n", self);
                tree.push_str(s.as_str());
                let mut ch_iter = children.iter().peekable();
                while let Some(c) = ch_iter.next() {
                    if_chain! {
                        if let Some(c) = book.get(c);
                        then {
                            let is_last = ch_iter.peek().is_some();
                            tree.push_str(
                                format!("{}{}", indent, if is_last { "├── " } else { "└── " }).as_str(),
                            );
                            c.tree_rec(
                                book,
                                tree,
                                format!("{}{}   ", indent, if is_last { "|" } else { " " }).as_str(),
                            );
                        }
                    }
                }
            }
        }
    }
}
