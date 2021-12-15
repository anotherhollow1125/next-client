use crate::errors::NcsError::*;
use crate::meta::*;
use crate::repair::ModifiedPath;
use crate::*;
use anyhow::{Context, Result};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{Method, Url};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::io;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use urlencoding::decode;

pub const WEBDAV_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind  xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
  <d:prop>
        <d:getetag />
        <d:getcontenttype />
  </d:prop>
</d:propfind>
"#;

#[derive(Clone, Debug)]
pub struct NCState {
    pub latest_activity_id: String,
}

impl NCState {
    pub fn eq_or_newer_than(&self, other: &Self) -> bool {
        let s = self
            .latest_activity_id
            .parse::<usize>()
            .expect("latest activity id val is invalid.");
        let o = other
            .latest_activity_id
            .parse::<usize>()
            .expect("latest activity id val is invalid.");

        o <= s
    }
}

pub async fn from_nc(nc_info: &NCInfo, local_info: &LocalInfo, target: &str) -> Result<Entry> {
    let target = add_head_slash(&target);
    let responses = comm_nc(nc_info, local_info, &target).await?;

    let target_name = path2name(&target);
    let target_name_without_slash = drop_slash(&target_name, &RE_HAS_LAST_SLASH);

    debug!("target_name: {}", target_name);

    let target_res = responses
        .into_iter()
        .filter(|r| {
            debug!("r_name: {}", &r.get_name());
            r.get_raw_name().as_str() == &target_name_without_slash
        })
        .nth(0);

    target_res.with_context(|| format!("Can not find target Entry."))
}

pub async fn ncpath_is_file(nc_info: &NCInfo, local_info: &LocalInfo, target: &str) -> bool {
    let entry_res = from_nc(nc_info, local_info, target).await;

    if let Ok(entry) = entry_res {
        entry.type_.is_file()
    } else {
        false
    }
}

pub async fn get_etag_from_nc(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    target: &str,
) -> Option<String> {
    let entry_res = from_nc(nc_info, local_info, target).await;

    if_chain! {
        if let Ok(entry) = entry_res;
        if let EntryType::File { etag } = entry.type_;
        then {
            etag
        } else {
            None
        }
    }
}

pub async fn from_nc_all(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    target: &str,
) -> Result<ArcEntry> {
    let target = add_head_slash(&target);
    let top_entry = from_nc(nc_info, local_info, &target).await?;
    let top_entry = Arc::new(Mutex::new(top_entry));

    get_children_rec(nc_info, local_info, &top_entry, "").await?;

    Ok(top_entry)
}

pub async fn from_nc_all_in_the_middle(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    target: &str,
) -> Result<ArcEntry> {
    let target = add_head_slash(&target);
    let top_entry = from_nc(nc_info, local_info, &target).await?;
    let top_entry = Arc::new(Mutex::new(top_entry));

    const RE_REMOVE_CHILDPART: Lazy<Regex> = Lazy::new(|| Regex::new("^(.*/)[^/]+$").unwrap());
    let p_str: &str = &RE_REMOVE_CHILDPART.replace(&target, "$1");

    get_children_rec(nc_info, local_info, &top_entry, p_str).await?;

    Ok(top_entry)
}

#[async_recursion]
async fn get_children_rec(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    parent_entry: &ArcEntry,
    ancestor_path: &str,
) -> Result<()> {
    let parent_name = parent_entry.lock().map_err(|_| LockError)?.get_name();
    let ancestor_path = format!("{}{}", ancestor_path, parent_name);
    let children_entries = comm_nc(nc_info, local_info, &ancestor_path)
        .await?
        .into_iter()
        .filter(|c| c.get_name().as_str() != &parent_name);

    for c in children_entries {
        let c = Arc::new(Mutex::new(c));
        get_children_rec(nc_info, local_info, &c, &ancestor_path).await?;
        Entry::append_child(parent_entry, c)?;
    }

    Ok(())
}

async fn comm_nc(nc_info: &NCInfo, local_info: &LocalInfo, target: &str) -> Result<Vec<Entry>> {
    let target = add_head_slash(target);
    let target = drop_slash(&target, &RE_HAS_LAST_SLASH);

    let path = format!("{}{}", &nc_info.root_path, target)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();

    let mut url = Url::parse(&nc_info.host)?;
    url.path_segments_mut().unwrap().extend(path);

    // let client = Client::builder().https_only(true).build()?;
    let ref client = local_info.req_client;

    let mut res_w = Err(anyhow!("dummy error"));
    for _ in 0u8..3 {
        let res = client
            .request(Method::from_bytes(b"PROPFIND").unwrap(), url.as_str())
            .basic_auth(&nc_info.username, Some(&nc_info.password))
            .header("Depth", "Infinity")
            .body(WEBDAV_BODY)
            .send()
            .await?;

        res_w = if res.status().is_success() {
            Ok(res)
        } else {
            Err(BadStatusError(res.status().as_u16()).into())
        };
        if res_w.is_ok() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let res = match res_w {
        Ok(r) => r,
        Err(e) => return Err(e),
    };

    let text = res.text_with_charset("utf-8").await?;

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = webdav_xml2responses(&document, &nc_info.root_path);
    let responses = responses
        .into_iter()
        .filter(|e| local_info.exc_checker.judge(e.get_raw_name()))
        .collect();

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
            let mut etag_w = None;
            let mut type_w = None;

            for m in n.children() {
                match m.tag_name().name() {
                    "href" => {
                        if let Some(href) = m.text() {
                            let path = href.replace(&root_path, "");
                            let path = decode(&path).ok()?;
                            name_w = Some(path2name(&path));
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
                                        _ => Some(EntryType::Directory),
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
                if let Some(etag) = etag_w;
                if let Some(type_) = type_w;
                then {
                    let type_ = if let EntryType::File {..} = type_ {
                        EntryType::File { etag: Some(etag) }
                    } else {
                        type_
                    };

                    Some(Entry::new(name, type_))
                } else {
                    None
                }
            }
        })
        .filter_map(|v| v)
        .collect()
}

// ==================== ↓ need refactoring ↓ ========================================
// This section has a lot of duplicate code above.
// but I'm reluctant to edit above code because it affect another section.

struct PathAndIsDir {
    path: String,
    is_dir: bool,
}

// best effort method.
async fn get_all_sub_path(nc_info: &NCInfo, local_info: &LocalInfo, target: &str) -> Vec<String> {
    let mut paths = HashSet::new();
    paths.insert(target.to_string());
    get_all_sub_path_rec(nc_info, local_info, target, &mut paths).await;
    let mut v = paths.into_iter().collect::<Vec<_>>();
    v.sort_by(|a, b| a.len().cmp(&b.len()));

    v
}

#[async_recursion]
async fn get_all_sub_path_rec(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    target: &str,
    paths: &mut HashSet<String>,
) {
    let parent_name = drop_slash(target, &RE_HAS_LAST_SLASH);
    let res = comm_nc_for_path(nc_info, local_info, &parent_name).await;
    let res = match res {
        Ok(v) => v,
        Err(e) => {
            error!("{:?}", e);
            return;
        }
    };
    let children_pathandisdirs = res.into_iter().filter(|c| {
        let child_path = drop_slash(&c.path, &RE_HAS_LAST_SLASH);
        child_path != parent_name
    });

    for PathAndIsDir { path, is_dir } in children_pathandisdirs {
        paths.insert(path.clone());
        if is_dir {
            get_all_sub_path_rec(nc_info, local_info, &path, paths).await;
        }
    }
}

async fn comm_nc_for_path(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    target: &str,
) -> Result<Vec<PathAndIsDir>> {
    let target = add_head_slash(target);
    let target = drop_slash(&target, &RE_HAS_LAST_SLASH);

    let path = format!("{}{}", &nc_info.root_path, target)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();

    let mut url = Url::parse(&nc_info.host)?;
    url.path_segments_mut().unwrap().extend(path);

    // let client = Client::builder().https_only(true).build()?;
    let ref client = local_info.req_client;

    let mut res_w = Err(anyhow!("dummy error"));
    for _ in 0u8..3 {
        let res = client
            .request(Method::from_bytes(b"PROPFIND").unwrap(), url.as_str())
            .basic_auth(&nc_info.username, Some(&nc_info.password))
            .header("Depth", "Infinity")
            .body(WEBDAV_BODY)
            .send()
            .await?;

        res_w = if res.status().is_success() {
            Ok(res)
        } else {
            Err(BadStatusError(res.status().as_u16()).into())
        };
        if res_w.is_ok() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let res = match res_w {
        Ok(r) => r,
        Err(e) => return Err(e),
    };

    let text = res.text_with_charset("utf-8").await?;

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = webdav_xml2paths(&document, &nc_info.root_path);
    let responses = responses
        .into_iter()
        .filter(|p| local_info.exc_checker.judge(&p.path))
        .collect();

    Ok(responses)
}

fn webdav_xml2paths(document: &roxmltree::Document, root_path: &str) -> Vec<PathAndIsDir> {
    document
        .root_element()
        .children()
        .map(|n| {
            if n.tag_name().name() != "response" {
                return None;
            }

            let mut path_w = None;
            let mut is_dir_w = None;

            for m in n.children() {
                match m.tag_name().name() {
                    "href" => {
                        if let Some(href) = m.text() {
                            let path = href.replace(&root_path, "");
                            let path = decode(&path).ok()?;
                            path_w = Some(path.to_string());
                        }
                    }
                    "propstat" => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "getcontenttype" => {
                                    is_dir_w = match d.text() {
                                        Some(ref s) if s != &"" => Some(false),
                                        _ => Some(true),
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
                if let Some(path) = path_w;
                if let Some(is_dir) = is_dir_w;
                then {
                    Some(PathAndIsDir { path: path.to_string(), is_dir })
                } else {
                    None
                }
            }
        })
        .filter_map(|v| v)
        .collect()
}

// ==================== ↑ need refactoring ↑ ========================================

fn save_file<R: io::Read + ?Sized>(
    r: &mut R,
    path: &str,
    local_info: &LocalInfo,
    stash: bool,
) -> Result<()> {
    let filename = format!("{}{}", local_info.root_path, path);
    fileope::save_file(r, &filename, stash, local_info)?;

    Ok(())
}

fn create_dir_all(path: &str, local_info: &LocalInfo) -> Result<()> {
    let dirname = format!("{}{}", local_info.root_path, path);
    fileope::create_dir_all(&dirname)?;

    Ok(())
}

fn touch_entry(path: &str, is_file: bool, local_info: &LocalInfo) -> Result<()> {
    let path = format!("{}{}", local_info.root_path, path);
    fileope::touch_entry(path, is_file)?;

    Ok(())
}

fn move_entry(from_path: &str, to_path: &str, stash: bool, local_info: &LocalInfo) -> Result<()> {
    let from_path = format!("{}{}", local_info.root_path, from_path);
    let to_path = format!("{}{}", local_info.root_path, to_path);
    fileope::move_entry(from_path, to_path, stash, local_info)?;

    Ok(())
}

fn remove_entry(path: &str, stash: bool, local_info: &LocalInfo) -> Result<()> {
    let path = format!("{}{}", local_info.root_path, path);
    fileope::remove_entry(path, stash, local_info)?;

    Ok(())
}

fn check_local_entry_is_dir(path: &str, local_info: &LocalInfo) -> bool {
    let path = format!("{}{}", local_info.root_path, path);

    Path::new(&path).is_dir()
}

#[async_recursion(?Send)]
pub async fn init_local_entries(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &ArcEntry,
    ancestor_path: &str,
) -> Result<()> {
    let mut entry_ref = entry.lock().map_err(|_| LockError)?;

    if entry_ref.status == EntryStatus::UpToDate {
        return Ok(());
    }

    let full_path = format!("{}{}", ancestor_path, entry_ref.get_name());
    match &entry_ref.type_ {
        &EntryType::Directory => {
            create_dir_all(&full_path, local_info)?;
            entry_ref.status = EntryStatus::UpToDate;
            for c in entry_ref.children.values() {
                init_local_entries(nc_info, local_info, c, &full_path).await?;
            }
        }
        &EntryType::File { .. } => {
            create_dir_all(ancestor_path, local_info)?;
            let res =
                download_file_in_rec(nc_info, local_info, &mut entry_ref, ancestor_path).await;
            match res {
                Ok(()) => {
                    entry_ref.status = EntryStatus::UpToDate;
                }
                Err(e) => {
                    entry_ref.status = EntryStatus::Error;
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}

async fn download_file_raw(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &mut Entry,
    full_path: &str,
    stash: bool,
) -> Result<()> {
    if entry.type_.is_dir() {
        return Err(anyhow!("Not file entry!!"));
    }

    let mut url = Url::parse(&nc_info.host)?;
    let path_v = format!("{}{}", nc_info.root_path, full_path)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    // let client = Client::builder().https_only(true).build()?;
    let ref client = local_info.req_client;

    let data_res = client
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .send()
        .await?;

    let new_etag = data_res
        .headers()
        .get("ETag")
        .with_context(|| format!("Can't get new etag."))
        .and_then(|v| v.to_str().with_context(|| "Can't get new etag."))
        .map(|v| v.to_string().replace("\"", ""))?;
    entry.type_ = EntryType::File {
        etag: Some(new_etag),
    };

    let bytes = data_res.bytes().await?;
    save_file(&mut bytes.as_ref(), full_path, local_info, stash)?;

    Ok(())
}

async fn download_file_in_rec(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &mut Entry,
    ancestor_path: &str,
) -> Result<()> {
    let full_path = format!("{}{}", ancestor_path, entry.get_name());

    download_file_raw(nc_info, local_info, entry, &full_path, false).await
}

pub async fn download_file_with_check_etag(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &ArcEntry,
    stash: bool,
) -> Result<Option<String>> {
    let full_path = Entry::get_path(entry)?;

    let nc_etag = get_etag_from_nc(nc_info, local_info, &full_path).await;

    {
        let mut entry_ref = entry.lock().map_err(|_| LockError)?;
        if_chain! {
            if let Some(etag) = nc_etag;
            if entry_ref.type_ != EntryType::File { etag: Some(etag) };
            then {
                debug!("Need to download.");
                download_file_raw(nc_info, local_info, &mut entry_ref, &full_path, stash).await?;
                return Ok(Some(full_path));
            }
        }
    }

    Ok(None)
}

pub async fn get_latest_activity_id(nc_info: &NCInfo, local_info: &LocalInfo) -> Result<String> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = OCS_ROOT
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    // let client = Client::builder().https_only(true).build()?;
    let ref client = local_info.req_client;

    let res = client
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("OCS-APIRequest", "true")
        .send()
        .await?;

    res.headers()
        .get("X-Activity-First-Known")
        .with_context(|| format!("Can't get latest activity id."))
        .and_then(|v| v.to_str().with_context(|| "Can't get latest activity id."))
        .map(|v| v.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NCEvent {
    Create(String),
    Delete(String),
    Modify(String),
    Move(String, String),
}

impl ModifiedPath for NCEvent {
    type Item = String;

    fn to_modified_path(&self) -> Option<String> {
        match self {
            Self::Create(s) => Some(s.clone()),
            Self::Delete(_) => None,
            Self::Modify(s) => Some(s.clone()),
            Self::Move(_, s) => Some(s.clone()),
        }
    }
}

pub async fn get_ncevents(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    nc_state: &mut NCState,
) -> Result<Vec<NCEvent>> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = OCS_ROOT
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    // let client = Client::builder().https_only(true).build()?;
    let ref client = local_info.req_client;

    let mut latest_activity_id = nc_state.latest_activity_id.to_string();
    let mut responses = vec![];
    let mut err = None;
    loop {
        let res = client
            .request(Method::GET, url.as_str())
            .query(&[("since", latest_activity_id.as_str()), ("sort", "asc")])
            .basic_auth(&nc_info.username, Some(&nc_info.password))
            .header("OCS-APIRequest", "true")
            .send()
            .await?;

        let s = res.status();
        if !s.is_success() {
            if s.as_u16() != 304 {
                err = Some(Err(BadStatusError(s.as_u16()).into()));
            }
            break;
            /*
            if s.as_u16() == 304 {
                return Ok(vec![]);
            } else {
                return Err(BadStatusError(s.as_u16()).into());
            }
            */
        }

        latest_activity_id = res
            .headers()
            .get("X-Activity-Last-Given")
            .with_context(|| format!("Can't get latest activity id."))
            .and_then(|v| v.to_str().with_context(|| "Can't get latest activity id."))
            .map(|v| v.to_string())?;

        let text = res.text_with_charset("utf-8").await?;

        let document: roxmltree::Document<'_> = roxmltree::Document::parse(&text)?;
        let mut r = ncevents_xml2responses(&document, nc_info, local_info).await?;
        responses.append(&mut r);
    }

    if_chain! {
        if let Some(e) = err;
        if latest_activity_id == "";
        then {
            e
        } else {
            nc_state.latest_activity_id = latest_activity_id;

            Ok(responses)
        }
    }
}

static RE_FILE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^file.*").unwrap());
static RE_NEWFILE: Lazy<Regex> = Lazy::new(|| Regex::new("^newfile.*").unwrap());
static RE_OLDFILE: Lazy<Regex> = Lazy::new(|| Regex::new("^oldfile.*").unwrap());

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ActivityType {
    FileCreated,
    FileRestored,
    FileChanged,
    FileDeleted,
}

async fn ncevents_xml2responses(
    document: &roxmltree::Document<'_>,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
) -> Result<Vec<NCEvent>> {
    let data = document
        .root_element()
        .children()
        .filter(|n| n.tag_name().name() == "data")
        .nth(0)
        .with_context(|| InvalidXMLError)?;

    let mut res = Vec::new();

    for n in data.children() {
        if n.tag_name().name() != "element" {
            continue;
        }

        let mut files = Vec::new();
        let mut new_files = Vec::new();
        let mut old_files = Vec::new();
        let mut activity_type = None;

        for m in n.children() {
            if m.tag_name().name() == "type" {
                activity_type = match m.text() {
                    Some("file_created") => Some(ActivityType::FileCreated),
                    Some("file_restored") => Some(ActivityType::FileRestored),
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

        old_files.sort_by(|a, b| a.len().cmp(&b.len()).reverse());

        let mut v = match activity_type {
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
            Some(ActivityType::FileRestored) => {
                let mut v = Vec::new();
                for f in files {
                    let mut t = get_all_sub_path(nc_info, local_info, &f)
                        .await
                        .into_iter()
                        .map(|p| NCEvent::Create(p))
                        .collect::<Vec<_>>();
                    v.append(&mut t);
                }
                v
            }
            None => Vec::new(),
        };
        res.append(&mut v);
    }

    Ok(res)
}

fn touch_targets(update_targets: Vec<WeakEntry>, local_info: &LocalInfo) -> anyhow::Result<()> {
    for target in update_targets.into_iter() {
        if let Some(e) = target.upgrade() {
            let path = Entry::get_path(&e)?;
            let mut e_ref = e.lock().map_err(|_| LockError)?;
            let res = touch_entry(&path, e_ref.type_.is_file(), local_info);
            match res {
                Ok(_) => {
                    e_ref.status = EntryStatus::UpToDate;
                }
                Err(er) => {
                    e_ref.status = EntryStatus::Error;
                    warn!("{:?}", er);
                }
            }
        }
    }

    Ok(())
}

async fn update_tree(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    mut nc_events: Vec<NCEvent>,
    root_entry: &ArcEntry,
    stash: bool,
) -> Result<Vec<WeakEntry>> {
    let mut download_targets = Vec::new();

    nc_events.reverse();
    while let Some(event) = nc_events.pop() {
        {
            let root_ref = root_entry.lock().map_err(|_| LockError)?;
            debug!("\n{}", root_ref.get_tree());
        }

        match event {
            NCEvent::Create(path) => {
                debug!("Create {}", path);

                if !local_info.exc_checker.judge(&path) {
                    debug!("{:?}: Excluded File.", path);
                    continue;
                }

                let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
                if Entry::get(root_entry, &path)?.is_some() {
                    continue;
                }
                let name = path2name(&path);
                let type_ = if ncpath_is_file(nc_info, local_info, &path).await {
                    EntryType::File { etag: None }
                } else {
                    EntryType::Directory
                };
                let new_entry = Arc::new(Mutex::new(Entry::new(name, type_)));
                let update_targets = Entry::append(
                    root_entry,
                    &path,
                    new_entry.clone(),
                    AppendMode::Create,
                    false,
                )?;
                let filop_res = touch_targets(update_targets, local_info);
                if let Err(e) = filop_res {
                    warn!("{:?}", e);
                }
                {
                    let mut new_entry_ref = new_entry.lock().map_err(|_| LockError)?;
                    if new_entry_ref.type_.is_file() {
                        new_entry_ref.status = EntryStatus::NeedUpdate;
                        let new_entry_w = Arc::downgrade(&new_entry);
                        download_targets.push(new_entry_w);
                    }
                }
            }
            NCEvent::Delete(path) => {
                debug!("Delete {}", path);

                if !local_info.exc_checker.judge(&path) {
                    debug!("{:?}: Excluded File.", path);
                    continue;
                }

                let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
                let mut root_ref = root_entry.lock().map_err(|_| LockError)?;
                let _ = root_ref.pop(&path)?;
                let filop_res = remove_entry(&path, stash, local_info);
                if let Err(e) = filop_res {
                    warn!("{:?}", e);
                }
            }
            NCEvent::Modify(path) => {
                debug!("Modify {}", path);

                if !local_info.exc_checker.judge(&path) {
                    debug!("{:?}: Excluded File.", path);
                    continue;
                }

                let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
                let w = Entry::get(root_entry, &path)?;
                if_chain! {
                    if let Some(w) = w;
                    if let Some(e) = w.upgrade();
                    then {
                        let is_file = {
                            let mut e_ref = e.lock().map_err(|_| LockError)?;
                            debug!("NeedUpdate substituted.");
                            e_ref.status = EntryStatus::NeedUpdate;
                            e_ref.type_.is_file()
                        };
                        if is_file {
                            download_targets.push(Arc::downgrade(&e));
                        }
                    }
                }
            }
            NCEvent::Move(from_path, to_path) => {
                debug!("Move {} => {}", from_path, to_path);

                if !local_info.exc_checker.judge(&to_path) {
                    debug!("{:?}: Excluded File.", to_path);
                    continue;
                }

                let to_path = drop_slash(&to_path, &RE_HAS_LAST_SLASH);
                let from_path = drop_slash(&from_path, &RE_HAS_LAST_SLASH);
                let target_w = {
                    let mut root_ref = root_entry.lock().map_err(|_| LockError)?;
                    root_ref.pop(&from_path)?
                };
                // .with_context(|| InvalidPathError(format!("{} => {}", from_path, to_path)));

                let target = match target_w {
                    Some(v) => v,
                    None => {
                        warn!(
                            "from_path({}) is not found.\nI'll try create entries. but This operation will cause strange result.",
                            from_path
                        );
                        let mut v = get_all_sub_path(nc_info, local_info, &to_path).await;
                        v.reverse();
                        for p in v {
                            nc_events.push(NCEvent::Create(p));
                        }
                        continue;
                    }
                };

                let update_targets =
                    Entry::append(root_entry, &to_path, target.clone(), AppendMode::Move, true)?;

                touch_targets(update_targets, local_info)?;

                let res = move_entry(&from_path, &to_path, stash, local_info);
                match res {
                    Err(er) => {
                        warn!("{:?}", er);
                    }
                    Ok(_) => {
                        debug!("try_fix_entry_type@move");
                        fix_entry_type_rec(&target, &mut download_targets, nc_info, local_info)
                            .await?;
                        /*
                        let path = Entry::get_path(&target)?;
                        let have_to_download =
                            try_fix_entry_type(&target, &path, nc_info, local_info).await?;
                        if have_to_download {
                            let w = Arc::downgrade(&target);
                            download_targets.push(w);
                        }
                        */
                    }
                }
            }
        }
    }
    Ok(download_targets)
}

#[async_recursion]
async fn fix_entry_type_rec(
    target: &ArcEntry,
    download_targets: &mut Vec<WeakEntry>,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
) -> Result<()> {
    let path = Entry::get_path(&target)?;
    let have_to_download = try_fix_entry_type(&target, &path, nc_info, local_info).await?;
    if have_to_download {
        let w = Arc::downgrade(&target);
        download_targets.push(w);
    }
    let (is_dir, children) = {
        let target_ref = target.lock().map_err(|_| LockError)?;
        (target_ref.type_.is_dir(), target_ref.get_all_children())
    };
    if is_dir {
        for child in children {
            if let Some(child) = child.upgrade() {
                let e = fix_entry_type_rec(&child, download_targets, nc_info, local_info).await;
                debug!("{:?}", e);
            }
        }
    }

    Ok(())
}

async fn try_fix_entry_type(
    target: &ArcEntry,
    path: &str,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
) -> Result<bool> {
    let res = from_nc(nc_info, local_info, path).await;

    let nc_entry = match res {
        Ok(entry) => entry,
        Err(_) => return Ok(false),
    };

    let mut target_ref = target.lock().map_err(|_| LockError)?;
    if target_ref.type_.is_same_type(&nc_entry.type_) {
        return Ok(false);
    }

    target_ref.status = EntryStatus::NeedUpdate;

    target_ref.type_ = if target_ref.type_.is_file() {
        EntryType::Directory
    } else {
        EntryType::File { etag: None }
    };

    debug!("{}", path);
    remove_entry(&path, false, local_info)?;
    debug!("removed. will touch");
    touch_entry(&path, target_ref.type_.is_file(), local_info)?;

    Ok(target_ref.type_.is_file())
}

pub async fn nclistening(
    tx: mpsc::Sender<Command>,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    mut nc_state: NCState,
) -> Result<()> {
    loop {
        if tx.is_closed() {
            return Ok(());
        }

        let dt = chrono::Local::now();
        let timestamp = dt.format("%Y-%m-%d %H:%M:%S").to_string();

        let mut contents = timestamp.as_bytes();
        let filename = local_info.get_keepalive_filename();
        let mut out = std::fs::File::create(&filename)?;
        io::copy(&mut contents, &mut out)?;

        if !network::check(&tx, nc_info, &local_info.req_client).await? {
            sleep(Duration::from_secs(10)).await;
            continue;
        }

        let events = get_ncevents(nc_info, local_info, &mut nc_state).await?;

        if events.len() > 0 {
            tx.send(Command::NCEvents(events, nc_state.clone())).await?;
        }

        sleep(Duration::from_secs(20)).await;
    }
}

pub async fn update_and_download(
    events: Vec<NCEvent>,
    root: &ArcEntry,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    nc2l_cancel_map: &mut HashMap<String, usize>,
    l2nc_cancel_set: &mut HashSet<NCEvent>,
    stash: bool,
) -> Result<()> {
    let events = events
        .into_iter()
        .filter(|ev| !l2nc_cancel_set.remove(ev))
        .collect::<Vec<_>>();
    debug!("events: {:?}", events);
    let download_targets = update_tree(nc_info, local_info, events, root, stash).await?;
    for target in download_targets.into_iter() {
        if let Some(e) = target.upgrade() {
            {
                let e_ref = e.lock().map_err(|_| LockError)?;
                if e_ref.type_.is_dir() {
                    continue;
                }
            }
            debug!("download target: {:?}", e);
            let r = download_file_with_check_etag(nc_info, local_info, &e, stash).await;
            let target_path = match r {
                Ok(path) => path,
                Err(e) => {
                    warn!("{:?}", e);
                    continue;
                }
            };
            {
                let mut e_ref = e.lock().map_err(|_| LockError)?;
                e_ref.status = EntryStatus::UpToDate;
            }
            if let Some(item) = target_path {
                let counter = nc2l_cancel_map.entry(item).or_insert(0);
                *counter += 1;
            }
        }
    }

    Ok(())
}

// possibly, there are similar functions above. sorry.
// つまりリファクタリングしたほうが良くない？関数ですハイ
// でもありそうでなかった関数かも...
pub async fn refresh<P>(
    target: P,
    is_recursive: bool,
    root: &ArcEntry,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    nc2l_cancel_map: &mut HashMap<String, usize>,
    stash: bool,
) -> Result<()>
where
    P: AsRef<Path> + Debug,
{
    let mut res = Ok(());

    let mut target_path = target.as_ref();
    if target_path.is_absolute() {
        target_path = match target_path.strip_prefix(&local_info.root_path_cano) {
            Ok(p) => p,
            Err(e) => return Err(anyhow!("Invalid Path. Please check the process. : {:?}", e)),
        }
    }

    let target_str = path2str(target_path);
    debug!("refresh beep {:?}", target_str);
    const RE_REMOVE_CHILDPART: Lazy<Regex> = Lazy::new(|| Regex::new("^(.*)/[^/]+$").unwrap());
    let p_str: &str = &RE_REMOVE_CHILDPART.replace(&target_str, "$1");

    let parent_entry = if_chain! {
        if let Ok(Some(w)) = Entry::get(root, p_str);
        if let Some(entry) = w.upgrade();
        then {
            entry
        } else {
            info!("[refresh] no such parent: {:?}", p_str);
            return Ok(());
        }
    };

    let target_entry = from_nc_all_in_the_middle(nc_info, local_info, &target_str).await?;
    Entry::append_child(&parent_entry, target_entry.clone())?;

    let is_file = {
        let entry = target_entry.lock().map_err(|_| LockError)?;
        entry.type_.is_file()
    };

    // If app had failed to get the file and the dir made instead.
    // It must repaired.
    // but this code section is not nessesary.
    // because thanks to the above code, new relary_entry is already gotten.
    /*
    if !is_file {
        let realy_entry = from_nc(nc_info, local_info, &target_str).await;
        if_chain! {
            if let Ok(r_entry) = realy_entry;
            if r_entry.type_.is_file();
            then {
                info!("fix type_ field of {:?}.", target_path);
                let mut entry = target_entry.lock().map_err(|_| LockError)?;
                entry.type_ = r_entry.type_;
                entry.clear_children_because_wrong_type();
                // remove_entry(&target_str, false, local_info)?;

                is_file = true;
            }
        }
    }
    */

    if is_file {
        remove_entry(&target_str, stash, local_info)?;

        {
            let mut entry = target_entry.lock().map_err(|_| LockError)?;
            // already stashed pre-process.
            download_file_raw(nc_info, local_info, &mut entry, &target_str, false).await?;
        }
        let counter = nc2l_cancel_map.entry(target_str).or_insert(0);
        *counter += 1;
    } else {
        if !check_local_entry_is_dir(&target_str, local_info) {
            remove_entry(&target_str, true, local_info)?;
            touch_entry(&target_str, false, local_info)?;
        }

        let children = {
            let entry = target_entry.lock().map_err(|_| LockError)?;
            entry.get_all_children()
        };

        for child in children.into_iter() {
            if let Some(child) = child.upgrade() {
                let r = refresh_rec(
                    &child,
                    &target_str,
                    is_recursive,
                    nc_info,
                    local_info,
                    nc2l_cancel_map,
                    stash,
                )
                .await;
                if let Err(e) = r {
                    res = Err(e);
                }
            }
        }
    }

    res
}

#[async_recursion(?Send)]
async fn refresh_rec(
    target_entry: &ArcEntry,
    parent_str: &str,
    is_recursive: bool,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    nc2l_cancel_map: &mut HashMap<String, usize>,
    stash: bool,
) -> Result<()> {
    let mut res = Ok(());

    let (name, is_file) = {
        let entry = target_entry.lock().map_err(|_| LockError)?;
        (entry.get_raw_name(), entry.type_.is_file())
    };

    let target_str = format!("{}/{}", parent_str, name);

    if is_file {
        remove_entry(&target_str, stash, local_info)?;

        {
            let mut entry = target_entry.lock().map_err(|_| LockError)?;
            // already stashed pre-process.
            download_file_raw(nc_info, local_info, &mut entry, &target_str, false).await?;
        }
        let counter = nc2l_cancel_map.entry(target_str).or_insert(0);
        *counter += 1;
    } else if is_recursive {
        if !check_local_entry_is_dir(&target_str, local_info) {
            remove_entry(&target_str, true, local_info)?;
            touch_entry(&target_str, false, local_info)?;
        }

        let children = {
            let entry = target_entry.lock().map_err(|_| LockError)?;
            entry.get_all_children()
        };

        for child in children.into_iter() {
            if let Some(child) = child.upgrade() {
                let r = refresh_rec(
                    &child,
                    &target_str,
                    is_recursive,
                    nc_info,
                    local_info,
                    nc2l_cancel_map,
                    stash,
                )
                .await;
                if let Err(e) = r {
                    res = Err(e);
                }
            }
        }
    }

    res
}
