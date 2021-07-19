use once_cell::sync::Lazy;
use regex::Regex;
use std::cell::RefCell;
use std::fmt;
use std::rc::{Rc, Weak};

#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate if_chain;

pub mod nc_listen;

pub static RE_HAS_LAST_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("(.*)/$").unwrap());
pub static RE_HAS_HEAD_SLASH: Lazy<Regex> = Lazy::new(|| Regex::new("^/(.*)").unwrap());

pub const WEBDAV_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind  xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
  <d:prop>
        <d:getetag />
        <d:getcontenttype />
  </d:prop>
</d:propfind>
"#;

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

type RcEntry = Rc<RefCell<Entry>>;
type WeakEntry = Weak<RefCell<Entry>>;

#[derive(Debug, Clone)]
pub enum EntryType {
    File,
    Directory { children: Vec<WeakEntry> },
}

impl EntryType {
    pub fn is_file(&self) -> bool {
        match self {
            &Self::File => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub etag: String,
    pub type_: EntryType,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.path_eq(other) && self.etag_eq(other)
    }
}

impl Eq for Entry {}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let etag_len = if self.etag.len() >= 8 {
            8
        } else {
            self.etag.len()
        };
        write!(f, "{} ({})", self.name, &self.etag[..etag_len])
    }
}

impl Entry {
    fn path_eq(&self, other: &Self) -> bool {
        self.name == other.name
    }

    fn etag_eq(&self, other: &Self) -> bool {
        self.etag == other.etag
    }

    pub fn get_tree(&self) -> String {
        let mut res = String::new();

        self.tree_rec(&mut res, "");

        res
    }

    fn tree_rec(&self, tree: &mut String, indent: &str) {
        match self.type_ {
            EntryType::File => {
                let s = format!("{}\n", self);
                tree.push_str(s.as_str());
            }
            EntryType::Directory { ref children } => {
                let s = format!("{}\n", self);
                tree.push_str(s.as_str());
                let mut ch_iter = children.iter().peekable();
                while let Some(c) = ch_iter.next() {
                    if_chain! {
                        if let Some(c) = c.upgrade();
                        if let Ok(c) = c.try_borrow();
                        then {
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
            }
        }
    }
}
