use anyhow::Context;
use log::debug;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{Client, Method, Url};
use std::collections::{HashMap, HashSet};
use std::io;
use urlencoding::decode;
// use std::cell::RefCell;
// use std::rc::Rc;

use crate::*;

const NC_ROOT_PREFIX: &str = "/remote.php/dav/files/";
pub const OCS_ROOT: &str = "/ocs/v2.php/apps/activity/api/v2/activity/all";

pub const WEBDAV_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind  xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
  <d:prop>
        <d:getetag />
        <d:getcontenttype />
  </d:prop>
</d:propfind>
"#;

pub struct NCInfo {
    pub username: String,
    pub password: String,
    pub host: String,
    pub root_path: String,
}

impl NCInfo {
    pub fn new(username: String, password: String, host: String) -> Self {
        let host = fix_host(&host);
        let root_path = format!("{}{}", NC_ROOT_PREFIX, username);
        let root_path = fix_root(&root_path);
        Self {
            username,
            password,
            host,
            root_path,
        }
    }
}

pub struct NCState {
    pub latest_activity_id: String,
}

pub async fn from_nc(nc_info: &NCInfo, target: &str) -> anyhow::Result<Entry> {
    let target = add_head_slash(&target);
    let responses = comm_nc(nc_info, &target).await?;

    let target_res = responses
        .into_iter()
        .filter(|r| {
            let a = drop_slash(&r.path, &RE_HAS_LAST_SLASH);
            let b = drop_slash(&target, &RE_HAS_LAST_SLASH);
            a == b
        })
        .nth(0);

    target_res.ok_or(anyhow!("Can not found target Entry."))
}

pub async fn from_nc_all(
    nc_info: &NCInfo,
    target: &str,
) -> anyhow::Result<(String, HashMap<String, Entry>)> {
    let target = add_head_slash(&target);
    let top_entry = from_nc(nc_info, &target).await?;
    let top_path = top_entry.path.clone();

    let mut book = HashMap::new();
    book.insert(top_path.clone(), top_entry);

    let mut stack = vec![top_path.clone()];
    while let Some(parent_path) = stack.pop() {
        get_children(nc_info, &parent_path, &mut book, &mut stack).await?;
    }

    Ok((top_path, book))
}

async fn get_children(
    nc_info: &NCInfo,
    parent_path: &str,
    book: &mut HashMap<String, Entry>,
    stack: &mut Vec<String>,
) -> anyhow::Result<()> {
    if_chain! {
        if let Some(p) = book.get(parent_path);
        if p.type_.is_file();
        then {
            return Ok(());
        }
    }

    let children_entries = comm_nc(nc_info, &parent_path)
        .await?
        .into_iter()
        .filter(|c| c.path != parent_path);

    let mut children = HashSet::new();
    for c in children_entries {
        let path = c.path.clone();

        if !c.type_.is_file() {
            stack.push(path.clone());
        }
        book.insert(path.clone(), c);

        children.insert(path);
    }

    if let Some(parent) = book.get_mut(parent_path) {
        parent.type_ = EntryType::Directory { children };
    }

    Ok(())
}

async fn comm_nc(nc_info: &NCInfo, target: &str) -> anyhow::Result<Vec<Entry>> {
    /*
    let host = fix_host(host);
    let root_path = fix_root(root_path);
    */
    let target = add_head_slash(target);
    // let target = drop_slash(target, &RE_HAS_HEAD_SLASH);

    let path = format!("{}{}", &nc_info.root_path, target)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();

    let mut url = Url::parse(&nc_info.host)?;
    url.path_segments_mut().unwrap().extend(path);

    let res = Client::new()
        .request(Method::from_bytes(b"PROPFIND").unwrap(), url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("Depth", "Infinity")
        .body(WEBDAV_BODY)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(anyhow!("status: {}", res.status()));
    }

    let text = res.text_with_charset("utf-8").await?;

    // debug!("{}", text);

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = webdav_xml2responses(&document, &nc_info.root_path);

    Ok(responses)
}

fn webdav_xml2responses(document: &roxmltree::Document, root_path: &str) -> Vec<Entry> {
    document
        .root_element()
        .children()
        .map(|n| {
            if n.tag_name().name() != "response" {
                return None;
            }

            let mut name_w = None;
            let mut path_w = None;
            let mut etag_w = None;
            let mut type_w = None;

            for m in n.children() {
                match m.tag_name().name() {
                    "href" => {
                        if let Some(href) = m.text() {
                            let path = href.replace(&root_path, "");
                            let path = decode(&path).ok()?;
                            let path_name = drop_slash(&path, &RE_HAS_LAST_SLASH);
                            name_w = Some(path_name.split("/").last().unwrap_or("").to_string());
                            path_w = Some(path_name);
                        }
                    }
                    "propstat" => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "getetag" => {
                                    etag_w = d.text().and_then(|s| Some(s.replace("\"", "")));
                                }
                                "getcontenttype" => {
                                    type_w = match d.text() {
                                        Some(ref s) if s != &"" => {
                                            Some(EntryType::File { etag: None })
                                        }
                                        _ => Some(EntryType::Directory {
                                            children: HashSet::new(),
                                        }),
                                    };
                                }
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
            }
            if_chain! {
                if let Some(name) = name_w;
                if let Some(path) = path_w;
                if let Some(etag) = etag_w;
                if let Some(type_) = type_w;
                then {
                    let (name, path) = match &type_ {
                        &EntryType::File {..} => (name, path),
                        _ => (add_last_slash(&name), add_last_slash(&path)),
                    };

                    // ルートディレクトリに限らず、親ディレクトリとETagが一致する現象が存在
                    // 一意性をもたせるのにETagは不十分そう
                    /*
                    let etag = if name == "" {
                        "".to_string()
                    } else {
                        etag
                    };
                    */

                    let type_ = if let EntryType::File {..} = type_ {
                        EntryType::File { etag: Some(etag) }
                    } else {
                        type_
                    };

                    Some(Entry {
                        name,
                        path,
                        // etag,
                        status: EntryStatus::NeedUpdate,
                        type_,
                    })
                } else {
                    None
                }
            }
        })
        .filter_map(|v| v)
        .collect()
}

/*
pub async fn make_entry_for_init(
    target_path: &str,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    book: &mut HashMap<String, Entry>,
) -> anyhow::Result<()> {
    let target_path = add_head_slash(target_path);
    let re = Regex::new("(.*)/.*?$").unwrap();

    if book.get(&target_path).is_none() {
        let mut path = re.replace(&target_path, "$1").to_string();
        while path.len() > 0 {
            if book.get(&path).is_some() {
                let (_, sub_book) = from_nc_all(nc_info, &path).await?;
                sub_book.into_iter().for_each(|(k, v)| {
                    book.insert(k, v);
                });
                break;
            }

            path = re.replace(&path, "$1").to_string();
        }
    }

    if book.get(&target_path).is_none() {
        return Err(anyhow!("No such target file."));
    }
    let mut target_entry = book.get_mut(&target_path).unwrap();

    let dir_path = re.replace(&target_path, "$1").to_string();
    let dir_path = format!("{}{}", local_info.root_path, dir_path);

    fileope::create_dir_all(&dir_path)?;

    if target_entry.type_.is_file() {
        download_file(&mut target_entry, &nc_info, local_info).await?;
    }

    Ok(())
}
*/

fn save_file<R: io::Read>(r: &mut R, path: &str, local_info: &LocalInfo) -> anyhow::Result<()> {
    let filename = format!("{}{}", local_info.root_path, path);
    fileope::save_file(r, &filename)?;

    Ok(())
}

fn create_dir_all(path: &str, local_info: &LocalInfo) -> anyhow::Result<()> {
    let dirname = format!("{}{}", local_info.root_path, path);
    fileope::create_dir_all(&dirname)?;

    Ok(())
}

pub async fn init_local_entries(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    book: &mut HashMap<String, Entry>,
) -> anyhow::Result<()> {
    let paths = book
        .iter()
        .map(|(k, v)| (k.clone(), v.type_.is_file()))
        .collect::<Vec<_>>();
    for (p, is_file) in paths {
        if is_file {
            init_local_file(&p, nc_info, local_info, book).await?;
        } else {
            init_local_dir(&p, local_info, book)?;
        }
    }
    Ok(())
}

pub fn init_local_dir(
    target_path: &str,
    local_info: &LocalInfo,
    book: &mut HashMap<String, Entry>,
) -> anyhow::Result<()> {
    let e_w = book.get_mut(target_path);
    if e_w.is_none() {
        return Err(anyhow!("{} : No such nc entry.", target_path));
    }
    let e = e_w.unwrap();

    if e.status == EntryStatus::UpToDate {
        return Ok(());
    }

    create_dir_all(e.path.as_str(), local_info)?;
    let parent_name_w = e.get_parent_name();
    e.status = EntryStatus::UpToDate;

    if let Some(parent_name) = parent_name_w {
        init_local_dir(&parent_name, local_info, book)?;
    }

    Ok(())
}

pub async fn init_local_file(
    target_path: &str,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    book: &mut HashMap<String, Entry>,
) -> anyhow::Result<()> {
    let e_w = book.get_mut(target_path);
    if e_w.is_none() {
        return Err(anyhow!("{} : No such nc entry.", target_path));
    }
    let e = e_w.unwrap();

    if e.status == EntryStatus::UpToDate {
        return Ok(());
    }

    if !e.type_.is_file() {
        return Err(anyhow!("Not File."));
    }

    let parent_name = e.get_parent_name().ok_or(anyhow!("Invalid File."))?;

    create_dir_all(&parent_name, local_info)?;
    download_file(e, nc_info, local_info).await?;
    e.status = EntryStatus::UpToDate;
    init_local_dir(&parent_name, local_info, book)?;

    Ok(())
}

async fn download_file(
    target: &mut Entry,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
) -> anyhow::Result<()> {
    if !target.type_.is_file() {
        return Err(anyhow!("Not file entry!!"));
    }

    let mut url = Url::parse(&nc_info.host)?;
    let path_v = format!("{}{}", nc_info.root_path, target.path)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let data_res = Client::new()
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .send()
        .await?;

    let new_etag = data_res
        .headers()
        .get("ETag")
        .ok_or(anyhow!("Can't get new etag."))
        .and_then(|v| v.to_str().with_context(|| "Can't get new etag."))
        .map(|v| v.to_string().replace("\"", ""))?;
    target.type_ = EntryType::File {
        etag: Some(new_etag),
    };

    let bytes = data_res.bytes().await?;
    save_file(&mut bytes.as_ref(), target.path.as_str(), local_info)?;

    Ok(())
}

pub async fn get_latest_activity_id(nc_info: &NCInfo) -> anyhow::Result<String> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = OCS_ROOT
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let res = Client::new()
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("OCS-APIRequest", "true")
        .send()
        .await?;

    res.headers()
        .get("X-Activity-First-Known")
        .ok_or(anyhow!("Can't get latest activity id."))
        .and_then(|v| v.to_str().with_context(|| "Can't get latest activity id."))
        .map(|v| v.to_string())
}

#[derive(Debug)]
pub enum NCEvent {
    Create(String),
    Delete(String),
    Modify(String),
    Move(String, String),
}

pub async fn get_ncevents(
    nc_info: &NCInfo,
    nc_state: &mut NCState,
) -> anyhow::Result<Vec<NCEvent>> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = OCS_ROOT
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let res = Client::new()
        .request(Method::GET, url.as_str())
        .query(&[
            ("since", nc_state.latest_activity_id.to_string().as_str()),
            ("sort", "asc"),
        ])
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("OCS-APIRequest", "true")
        .send()
        .await?;

    let s = res.status();
    if !s.is_success() {
        if s.as_u16() == 304 {
            return Ok(vec![]);
        } else {
            return Err(anyhow!("status: {}", s));
        }
    }

    let latest_activity_id = res
        .headers()
        .get("X-Activity-Last-Given")
        .ok_or(anyhow!("Can't get latest activity id."))
        .and_then(|v| v.to_str().with_context(|| "Can't get latest activity id."))
        .map(|v| v.to_string())?;

    let text = res.text_with_charset("utf-8").await?;

    debug!("{}", text);

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = ncevents_xml2responses(&document)?;

    nc_state.latest_activity_id = latest_activity_id;

    Ok(responses)
}

static RE_FILE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^file.*").unwrap());
static RE_NEWFILE: Lazy<Regex> = Lazy::new(|| Regex::new("^newfile.*").unwrap());
static RE_OLDFILE: Lazy<Regex> = Lazy::new(|| Regex::new("^oldfile.*").unwrap());

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ActivityType {
    FileCreated,
    FileChanged,
    FileDeleted,
}

fn ncevents_xml2responses(document: &roxmltree::Document) -> anyhow::Result<Vec<NCEvent>> {
    let data = document
        .root_element()
        .children()
        .filter(|n| n.tag_name().name() == "data")
        .nth(0)
        .ok_or(anyhow!("Invalid XML."))?;

    let res = data
        .children()
        .map(|n| {
            if n.tag_name().name() != "element" {
                return Vec::new();
            }

            let mut files = Vec::new();
            let mut new_files = Vec::new();
            let mut old_files = Vec::new();
            let mut activity_type = None;

            for m in n.children() {
                if m.tag_name().name() == "type" {
                    activity_type = match m.text() {
                        Some("file_created") | Some("file_restored") => {
                            Some(ActivityType::FileCreated)
                        }
                        Some("file_changed") => Some(ActivityType::FileChanged),
                        Some("file_deleted") => Some(ActivityType::FileDeleted),
                        _ => None,
                    };
                }
            }

            for m in n.descendants() {
                match m.tag_name().name() {
                    s if RE_FILE.is_match(s) => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "path" => {
                                    let path = d.text().unwrap_or("");
                                    let path = add_head_slash(path);
                                    files.push(path);
                                }
                                _ => (),
                            }
                        }
                    }
                    s if RE_NEWFILE.is_match(s) => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "path" => {
                                    let path = d.text().unwrap_or("");
                                    let path = add_head_slash(path);
                                    new_files.push(path);
                                }
                                _ => (),
                            }
                        }
                    }
                    s if RE_OLDFILE.is_match(s) => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "path" => {
                                    let path = d.text().unwrap_or("");
                                    let path = add_head_slash(path);
                                    old_files.push(path);
                                }
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
            }

            debug!(
                "{:?} {:?} {:?} {:?}",
                activity_type, files, new_files, old_files
            );

            match activity_type {
                Some(ActivityType::FileCreated) => {
                    files.into_iter().map(|f| NCEvent::Create(f)).collect()
                }
                Some(ActivityType::FileDeleted) => {
                    files.into_iter().map(|f| NCEvent::Delete(f)).collect()
                }
                Some(ActivityType::FileChanged) => {
                    if new_files.len() > 0 {
                        let new_file = new_files.into_iter().nth(0).unwrap();
                        old_files
                            .into_iter()
                            .map(|f| NCEvent::Move(f, new_file.clone()))
                            .collect()
                    } else {
                        files.into_iter().map(|f| NCEvent::Modify(f)).collect()
                    }
                }
                _ => Vec::new(),
            }
        })
        .flatten()
        .collect();

    Ok(res)
}

/*

// Rc使いたいRc使いたいRc使いたい(魂の叫び)
// マルチスレッドの文脈がありえるのでHashMapで耐えます...
fn update_tree(nc_events: Vec<NCEvent>, book: &mut HashMap<String, Entry>, update_cands: &mut HashSet<String>) -> anyhow::Result<()> {
    for event in nc_events.into_iter() {
        match event {
            NCEvent::Create(path) => todo!(),
            NCEvent::Delete(path) => todo!(),
            NCEvent::Modify(path) => {
                if let Some(entry) = book.get_mut(path) {
                    entry.status = EntryStatus::NeedUpdate;
                } else {
                }
            }
            NCEvent::Move(from_path, to_path) => todo!(),
        }
    }
    Ok(())
}

fn insert_entry(book: &mut HashMap<String, Entry>, ) -> anyhow::Result<()> {
    Ok(())
}

fn remove_entry(book: )
*/
